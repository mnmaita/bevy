#![expect(missing_docs, reason = "Not all docs are written yet, see #3492.")]
#![expect(unsafe_code, reason = "Unsafe code is used to improve performance.")]
#![cfg_attr(
    any(docsrs, docsrs_dep),
    expect(
        internal_features,
        reason = "rustdoc_internals is needed for fake_variadic"
    )
)]
#![cfg_attr(any(docsrs, docsrs_dep), feature(doc_auto_cfg, rustdoc_internals))]
#![doc(
    html_logo_url = "https://bevy.org/assets/icon.png",
    html_favicon_url = "https://bevy.org/assets/icon.png"
)]

#[cfg(target_pointer_width = "16")]
compile_error!("bevy_render cannot compile for a 16-bit platform.");

extern crate alloc;
extern crate core;

// Required to make proc macros work in bevy itself.
extern crate self as bevy_render;

pub mod alpha;
pub mod batching;
pub mod camera;
pub mod diagnostic;
pub mod erased_render_asset;
pub mod experimental;
pub mod extract_component;
pub mod extract_instances;
mod extract_param;
pub mod extract_resource;
pub mod globals;
pub mod gpu_component_array_buffer;
pub mod gpu_readback;
pub mod mesh;
#[cfg(not(target_arch = "wasm32"))]
pub mod pipelined_rendering;
pub mod render_asset;
pub mod render_graph;
pub mod render_phase;
pub mod render_resource;
pub mod renderer;
pub mod settings;
pub mod storage;
pub mod sync_component;
pub mod sync_world;
pub mod texture;
pub mod view;
mod wgpu_wrapper;
pub use bevy_camera::primitives;
#[cfg(feature = "bevy_light")]
mod extract_impls;

/// The render prelude.
///
/// This includes the most common types in this crate, re-exported for your convenience.
pub mod prelude {
    #[doc(hidden)]
    pub use crate::{
        alpha::AlphaMode,
        camera::ToNormalizedRenderTarget as _,
        mesh::{
            morph::MorphWeights, primitives::MeshBuilder, primitives::Meshable, Mesh, Mesh2d,
            Mesh3d,
        },
        render_resource::Shader,
        texture::{ImagePlugin, ManualTextureViews},
        view::{InheritedVisibility, Msaa, ViewVisibility, Visibility},
        ExtractSchedule,
    };
    // TODO: Remove this in a follow-up
    #[doc(hidden)]
    pub use bevy_camera::{
        Camera, ClearColor, ClearColorConfig, OrthographicProjection, PerspectiveProjection,
        Projection,
    };
}
use batching::gpu_preprocessing::BatchingPlugin;

#[doc(hidden)]
pub mod _macro {
    pub use bevy_asset;
}

use bevy_ecs::schedule::ScheduleBuildSettings;
use bevy_image::{CompressedImageFormatSupport, CompressedImageFormats};
use bevy_utils::prelude::default;
pub use extract_param::Extract;

use bevy_window::{PrimaryWindow, RawHandleWrapperHolder};
use experimental::occlusion_culling::OcclusionCullingPlugin;
use globals::GlobalsPlugin;
use render_asset::{
    extract_render_asset_bytes_per_frame, reset_render_asset_bytes_per_frame,
    RenderAssetBytesPerFrame, RenderAssetBytesPerFrameLimiter,
};
use renderer::{RenderAdapter, RenderDevice, RenderQueue};
use settings::RenderResources;
use sync_world::{
    despawn_temporary_render_entities, entity_sync_system, MainEntity, RenderEntity,
    SyncToRenderWorld, SyncWorldPlugin, TemporaryRenderEntity,
};

use crate::gpu_readback::GpuReadbackPlugin;
use crate::{
    camera::CameraPlugin,
    mesh::{MeshPlugin, MorphPlugin, RenderMesh},
    render_asset::prepare_assets,
    render_resource::{PipelineCache, Shader, ShaderLoader},
    renderer::{render_system, RenderInstance},
    settings::RenderCreation,
    storage::StoragePlugin,
    view::{ViewPlugin, WindowRenderPlugin},
};
use alloc::sync::Arc;
use bevy_app::{App, AppLabel, Plugin, SubApp};
use bevy_asset::{AssetApp, AssetServer};
use bevy_ecs::{prelude::*, schedule::ScheduleLabel};
use bitflags::bitflags;
use core::ops::{Deref, DerefMut};
use std::sync::Mutex;
use tracing::debug;
use wgpu_wrapper::WgpuWrapper;

/// Inline shader as an `embedded_asset` and load it permanently.
///
/// This works around a limitation of the shader loader not properly loading
/// dependencies of shaders.
#[macro_export]
macro_rules! load_shader_library {
    ($asset_server_provider: expr, $path: literal $(, $settings: expr)?) => {
        $crate::_macro::bevy_asset::embedded_asset!($asset_server_provider, $path);
        let handle: $crate::_macro::bevy_asset::prelude::Handle<$crate::prelude::Shader> =
            $crate::_macro::bevy_asset::load_embedded_asset!(
                $asset_server_provider,
                $path
                $(,$settings)?
            );
        core::mem::forget(handle);
    }
}

/// Contains the default Bevy rendering backend based on wgpu.
///
/// Rendering is done in a [`SubApp`], which exchanges data with the main app
/// between main schedule iterations.
///
/// Rendering can be executed between iterations of the main schedule,
/// or it can be executed in parallel with main schedule when
/// [`PipelinedRenderingPlugin`](pipelined_rendering::PipelinedRenderingPlugin) is enabled.
#[derive(Default)]
pub struct RenderPlugin {
    pub render_creation: RenderCreation,
    /// If `true`, disables asynchronous pipeline compilation.
    /// This has no effect on macOS, Wasm, iOS, or without the `multi_threaded` feature.
    pub synchronous_pipeline_compilation: bool,
    /// Debugging flags that can optionally be set when constructing the renderer.
    pub debug_flags: RenderDebugFlags,
}

bitflags! {
    /// Debugging flags that can optionally be set when constructing the renderer.
    #[derive(Clone, Copy, PartialEq, Default, Debug)]
    pub struct RenderDebugFlags: u8 {
        /// If true, this sets the `COPY_SRC` flag on indirect draw parameters
        /// so that they can be read back to CPU.
        ///
        /// This is a debugging feature that may reduce performance. It
        /// primarily exists for the `occlusion_culling` example.
        const ALLOW_COPIES_FROM_INDIRECT_PARAMETERS = 1;
    }
}

/// The systems sets of the default [`App`] rendering schedule.
///
/// These can be useful for ordering, but you almost never want to add your systems to these sets.
#[derive(Debug, Hash, PartialEq, Eq, Clone, SystemSet)]
pub enum RenderSystems {
    /// This is used for applying the commands from the [`ExtractSchedule`]
    ExtractCommands,
    /// Prepare assets that have been created/modified/removed this frame.
    PrepareAssets,
    /// Prepares extracted meshes.
    PrepareMeshes,
    /// Create any additional views such as those used for shadow mapping.
    ManageViews,
    /// Queue drawable entities as phase items in render phases ready for
    /// sorting (if necessary)
    Queue,
    /// A sub-set within [`Queue`](RenderSystems::Queue) where mesh entity queue systems are executed. Ensures `prepare_assets::<RenderMesh>` is completed.
    QueueMeshes,
    /// A sub-set within [`Queue`](RenderSystems::Queue) where meshes that have
    /// become invisible or changed phases are removed from the bins.
    QueueSweep,
    // TODO: This could probably be moved in favor of a system ordering
    // abstraction in `Render` or `Queue`
    /// Sort the [`SortedRenderPhase`](render_phase::SortedRenderPhase)s and
    /// [`BinKey`](render_phase::BinnedPhaseItem::BinKey)s here.
    PhaseSort,
    /// Prepare render resources from extracted data for the GPU based on their sorted order.
    /// Create [`BindGroups`](render_resource::BindGroup) that depend on those data.
    Prepare,
    /// A sub-set within [`Prepare`](RenderSystems::Prepare) for initializing buffers, textures and uniforms for use in bind groups.
    PrepareResources,
    /// Collect phase buffers after
    /// [`PrepareResources`](RenderSystems::PrepareResources) has run.
    PrepareResourcesCollectPhaseBuffers,
    /// Flush buffers after [`PrepareResources`](RenderSystems::PrepareResources), but before [`PrepareBindGroups`](RenderSystems::PrepareBindGroups).
    PrepareResourcesFlush,
    /// A sub-set within [`Prepare`](RenderSystems::Prepare) for constructing bind groups, or other data that relies on render resources prepared in [`PrepareResources`](RenderSystems::PrepareResources).
    PrepareBindGroups,
    /// Actual rendering happens here.
    /// In most cases, only the render backend should insert resources here.
    Render,
    /// Cleanup render resources here.
    Cleanup,
    /// Final cleanup occurs: all entities will be despawned.
    ///
    /// Runs after [`Cleanup`](RenderSystems::Cleanup).
    PostCleanup,
}

/// Deprecated alias for [`RenderSystems`].
#[deprecated(since = "0.17.0", note = "Renamed to `RenderSystems`.")]
pub type RenderSet = RenderSystems;

/// The startup schedule of the [`RenderApp`]
#[derive(ScheduleLabel, Debug, Hash, PartialEq, Eq, Clone, Default)]
pub struct RenderStartup;

/// The main render schedule.
#[derive(ScheduleLabel, Debug, Hash, PartialEq, Eq, Clone, Default)]
pub struct Render;

impl Render {
    /// Sets up the base structure of the rendering [`Schedule`].
    ///
    /// The sets defined in this enum are configured to run in order.
    pub fn base_schedule() -> Schedule {
        use RenderSystems::*;

        let mut schedule = Schedule::new(Self);

        schedule.configure_sets(
            (
                ExtractCommands,
                PrepareMeshes,
                ManageViews,
                Queue,
                PhaseSort,
                Prepare,
                Render,
                Cleanup,
                PostCleanup,
            )
                .chain(),
        );

        schedule.configure_sets((ExtractCommands, PrepareAssets, PrepareMeshes, Prepare).chain());
        schedule.configure_sets(
            (QueueMeshes, QueueSweep)
                .chain()
                .in_set(Queue)
                .after(prepare_assets::<RenderMesh>),
        );
        schedule.configure_sets(
            (
                PrepareResources,
                PrepareResourcesCollectPhaseBuffers,
                PrepareResourcesFlush,
                PrepareBindGroups,
            )
                .chain()
                .in_set(Prepare),
        );

        schedule
    }
}

/// Schedule which extract data from the main world and inserts it into the render world.
///
/// This step should be kept as short as possible to increase the "pipelining potential" for
/// running the next frame while rendering the current frame.
///
/// This schedule is run on the main world, but its buffers are not applied
/// until it is returned to the render world.
#[derive(ScheduleLabel, PartialEq, Eq, Debug, Clone, Hash, Default)]
pub struct ExtractSchedule;

/// The simulation [`World`] of the application, stored as a resource.
///
/// This resource is only available during [`ExtractSchedule`] and not
/// during command application of that schedule.
/// See [`Extract`] for more details.
#[derive(Resource, Default)]
pub struct MainWorld(World);

impl Deref for MainWorld {
    type Target = World;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MainWorld {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub mod graph {
    use crate::render_graph::RenderLabel;

    #[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
    pub struct CameraDriverLabel;
}

#[derive(Resource)]
struct FutureRenderResources(Arc<Mutex<Option<RenderResources>>>);

/// A label for the rendering sub-app.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, AppLabel)]
pub struct RenderApp;

impl Plugin for RenderPlugin {
    /// Initializes the renderer, sets up the [`RenderSystems`] and creates the rendering sub-app.
    fn build(&self, app: &mut App) {
        app.init_asset::<Shader>()
            .init_asset_loader::<ShaderLoader>();

        match &self.render_creation {
            RenderCreation::Manual(resources) => {
                let future_render_resources_wrapper = Arc::new(Mutex::new(Some(resources.clone())));
                app.insert_resource(FutureRenderResources(
                    future_render_resources_wrapper.clone(),
                ));
                // SAFETY: Plugins should be set up on the main thread.
                unsafe { initialize_render_app(app) };
            }
            RenderCreation::Automatic(render_creation) => {
                if let Some(backends) = render_creation.backends {
                    let future_render_resources_wrapper = Arc::new(Mutex::new(None));
                    app.insert_resource(FutureRenderResources(
                        future_render_resources_wrapper.clone(),
                    ));

                    let primary_window = app
                        .world_mut()
                        .query_filtered::<&RawHandleWrapperHolder, With<PrimaryWindow>>()
                        .single(app.world())
                        .ok()
                        .cloned();
                    let settings = render_creation.clone();
                    let async_renderer = async move {
                        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                            backends,
                            flags: settings.instance_flags,
                            backend_options: wgpu::BackendOptions {
                                gl: wgpu::GlBackendOptions {
                                    gles_minor_version: settings.gles3_minor_version,
                                    fence_behavior: wgpu::GlFenceBehavior::Normal,
                                },
                                dx12: wgpu::Dx12BackendOptions {
                                    shader_compiler: settings.dx12_shader_compiler.clone(),
                                },
                                noop: wgpu::NoopBackendOptions { enable: false },
                            },
                        });

                        let surface = primary_window.and_then(|wrapper| {
                            let maybe_handle = wrapper.0.lock().expect(
                                "Couldn't get the window handle in time for renderer initialization",
                            );
                            if let Some(wrapper) = maybe_handle.as_ref() {
                                // SAFETY: Plugins should be set up on the main thread.
                                let handle = unsafe { wrapper.get_handle() };
                                Some(
                                    instance
                                        .create_surface(handle)
                                        .expect("Failed to create wgpu surface"),
                                )
                            } else {
                                None
                            }
                        });

                        let force_fallback_adapter = std::env::var("WGPU_FORCE_FALLBACK_ADAPTER")
                            .map_or(settings.force_fallback_adapter, |v| {
                                !(v.is_empty() || v == "0" || v == "false")
                            });

                        let desired_adapter_name = std::env::var("WGPU_ADAPTER_NAME")
                            .as_deref()
                            .map_or(settings.adapter_name.clone(), |x| Some(x.to_lowercase()));

                        let request_adapter_options = wgpu::RequestAdapterOptions {
                            power_preference: settings.power_preference,
                            compatible_surface: surface.as_ref(),
                            force_fallback_adapter,
                        };

                        let (device, queue, adapter_info, render_adapter) =
                            renderer::initialize_renderer(
                                &instance,
                                &settings,
                                &request_adapter_options,
                                desired_adapter_name,
                            )
                            .await;
                        debug!("Configured wgpu adapter Limits: {:#?}", device.limits());
                        debug!("Configured wgpu adapter Features: {:#?}", device.features());
                        let mut future_render_resources_inner =
                            future_render_resources_wrapper.lock().unwrap();
                        *future_render_resources_inner = Some(RenderResources(
                            device,
                            queue,
                            adapter_info,
                            render_adapter,
                            RenderInstance(Arc::new(WgpuWrapper::new(instance))),
                        ));
                    };
                    // In wasm, spawn a task and detach it for execution
                    #[cfg(target_arch = "wasm32")]
                    bevy_tasks::IoTaskPool::get()
                        .spawn_local(async_renderer)
                        .detach();
                    // Otherwise, just block for it to complete
                    #[cfg(not(target_arch = "wasm32"))]
                    futures_lite::future::block_on(async_renderer);

                    // SAFETY: Plugins should be set up on the main thread.
                    unsafe { initialize_render_app(app) };
                }
            }
        };

        app.add_plugins((
            WindowRenderPlugin,
            CameraPlugin,
            ViewPlugin,
            MeshPlugin,
            GlobalsPlugin,
            MorphPlugin,
            BatchingPlugin {
                debug_flags: self.debug_flags,
            },
            SyncWorldPlugin,
            StoragePlugin,
            GpuReadbackPlugin::default(),
            OcclusionCullingPlugin,
            #[cfg(feature = "tracing-tracy")]
            diagnostic::RenderDiagnosticsPlugin,
        ));

        app.init_resource::<RenderAssetBytesPerFrame>();
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<RenderAssetBytesPerFrameLimiter>();
            render_app
                .add_systems(ExtractSchedule, extract_render_asset_bytes_per_frame)
                .add_systems(
                    Render,
                    reset_render_asset_bytes_per_frame.in_set(RenderSystems::Cleanup),
                );
        }

        app.register_type::<alpha::AlphaMode>()
            // These types cannot be registered in bevy_color, as it does not depend on the rest of Bevy
            .register_type::<bevy_color::Color>()
            .register_type::<RenderEntity>()
            .register_type::<TemporaryRenderEntity>()
            .register_type::<MainEntity>()
            .register_type::<SyncToRenderWorld>();
    }

    fn ready(&self, app: &App) -> bool {
        app.world()
            .get_resource::<FutureRenderResources>()
            .and_then(|frr| frr.0.try_lock().map(|locked| locked.is_some()).ok())
            .unwrap_or(true)
    }

    fn finish(&self, app: &mut App) {
        load_shader_library!(app, "maths.wgsl");
        load_shader_library!(app, "color_operations.wgsl");
        load_shader_library!(app, "bindless.wgsl");
        if let Some(future_render_resources) =
            app.world_mut().remove_resource::<FutureRenderResources>()
        {
            let RenderResources(device, queue, adapter_info, render_adapter, instance) =
                future_render_resources.0.lock().unwrap().take().unwrap();

            let compressed_image_format_support = CompressedImageFormatSupport(
                CompressedImageFormats::from_features(device.features()),
            );

            app.insert_resource(device.clone())
                .insert_resource(queue.clone())
                .insert_resource(adapter_info.clone())
                .insert_resource(render_adapter.clone())
                .insert_resource(compressed_image_format_support);

            let render_app = app.sub_app_mut(RenderApp);

            render_app
                .insert_resource(instance)
                .insert_resource(PipelineCache::new(
                    device.clone(),
                    render_adapter.clone(),
                    self.synchronous_pipeline_compilation,
                ))
                .insert_resource(device)
                .insert_resource(queue)
                .insert_resource(render_adapter)
                .insert_resource(adapter_info);
        }
    }
}

/// A "scratch" world used to avoid allocating new worlds every frame when
/// swapping out the [`MainWorld`] for [`ExtractSchedule`].
#[derive(Resource, Default)]
struct ScratchMainWorld(World);

/// Executes the [`ExtractSchedule`] step of the renderer.
/// This updates the render world with the extracted ECS data of the current frame.
fn extract(main_world: &mut World, render_world: &mut World) {
    // temporarily add the app world to the render world as a resource
    let scratch_world = main_world.remove_resource::<ScratchMainWorld>().unwrap();
    let inserted_world = core::mem::replace(main_world, scratch_world.0);
    render_world.insert_resource(MainWorld(inserted_world));
    render_world.run_schedule(ExtractSchedule);

    // move the app world back, as if nothing happened.
    let inserted_world = render_world.remove_resource::<MainWorld>().unwrap();
    let scratch_world = core::mem::replace(main_world, inserted_world.0);
    main_world.insert_resource(ScratchMainWorld(scratch_world));
}

/// # Safety
/// This function must be called from the main thread.
unsafe fn initialize_render_app(app: &mut App) {
    app.init_resource::<ScratchMainWorld>();

    let mut render_app = SubApp::new();
    render_app.update_schedule = Some(Render.intern());

    let mut extract_schedule = Schedule::new(ExtractSchedule);
    // We skip applying any commands during the ExtractSchedule
    // so commands can be applied on the render thread.
    extract_schedule.set_build_settings(ScheduleBuildSettings {
        auto_insert_apply_deferred: false,
        ..default()
    });
    extract_schedule.set_apply_final_deferred(false);

    render_app
        .add_schedule(extract_schedule)
        .add_schedule(Render::base_schedule())
        .init_resource::<render_graph::RenderGraph>()
        .insert_resource(app.world().resource::<AssetServer>().clone())
        .add_systems(ExtractSchedule, PipelineCache::extract_shaders)
        .add_systems(
            Render,
            (
                // This set applies the commands from the extract schedule while the render schedule
                // is running in parallel with the main app.
                apply_extract_commands.in_set(RenderSystems::ExtractCommands),
                (PipelineCache::process_pipeline_queue_system, render_system)
                    .chain()
                    .in_set(RenderSystems::Render),
                despawn_temporary_render_entities.in_set(RenderSystems::PostCleanup),
            ),
        );

    // We want the closure to have a flag to only run the RenderStartup schedule once, but the only
    // way to have the closure store this flag is by capturing it. This variable is otherwise
    // unused.
    let mut should_run_startup = true;
    render_app.set_extract(move |main_world, render_world| {
        if should_run_startup {
            // Run the `RenderStartup` if it hasn't run yet. This does mean `RenderStartup` blocks
            // the rest of the app extraction, but this is necessary since extraction itself can
            // depend on resources initialized in `RenderStartup`.
            render_world.run_schedule(RenderStartup);
            should_run_startup = false;
        }

        {
            #[cfg(feature = "trace")]
            let _stage_span = tracing::info_span!("entity_sync").entered();
            entity_sync_system(main_world, render_world);
        }

        // run extract schedule
        extract(main_world, render_world);
    });

    let (sender, receiver) = bevy_time::create_time_channels();
    render_app.insert_resource(sender);
    app.insert_resource(receiver);
    app.insert_sub_app(RenderApp, render_app);
}

/// Applies the commands from the extract schedule. This happens during
/// the render schedule rather than during extraction to allow the commands to run in parallel with the
/// main app when pipelined rendering is enabled.
fn apply_extract_commands(render_world: &mut World) {
    render_world.resource_scope(|render_world, mut schedules: Mut<Schedules>| {
        schedules
            .get_mut(ExtractSchedule)
            .unwrap()
            .apply_deferred(render_world);
    });
}

/// If the [`RenderAdapter`] is a Qualcomm Adreno, returns its model number.
///
/// This lets us work around hardware bugs.
pub fn get_adreno_model(adapter: &RenderAdapter) -> Option<u32> {
    if !cfg!(target_os = "android") {
        return None;
    }

    let adapter_name = adapter.get_info().name;
    let adreno_model = adapter_name.strip_prefix("Adreno (TM) ")?;

    // Take suffixes into account (like Adreno 642L).
    Some(
        adreno_model
            .chars()
            .map_while(|c| c.to_digit(10))
            .fold(0, |acc, digit| acc * 10 + digit),
    )
}

/// Get the Mali driver version if the adapter is a Mali GPU.
pub fn get_mali_driver_version(adapter: &RenderAdapter) -> Option<u32> {
    if !cfg!(target_os = "android") {
        return None;
    }

    let driver_name = adapter.get_info().name;
    if !driver_name.contains("Mali") {
        return None;
    }
    let driver_info = adapter.get_info().driver_info;
    if let Some(start_pos) = driver_info.find("v1.r") {
        if let Some(end_pos) = driver_info[start_pos..].find('p') {
            let start_idx = start_pos + 4; // Skip "v1.r"
            let end_idx = start_pos + end_pos;

            return driver_info[start_idx..end_idx].parse::<u32>().ok();
        }
    }

    None
}
