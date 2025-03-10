use basedrop::{Collector, Shared, SharedCell};
use fnv::FnvHashSet;
use meadowlark_core_types::time::{SampleRate, Seconds};
use smallvec::SmallVec;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use thread_priority::ThreadPriority;

use dropseed_plugin_api::ext::gui::{GuiResizeHints, GuiSize};
use dropseed_plugin_api::plugin_scanner::ScannedPluginKey;
use dropseed_plugin_api::transport::TempoMap;
use dropseed_plugin_api::{DSPluginSaveState, HostInfo, PluginFactory, PluginInstanceID};

use crate::engine::audio_thread::DSEngineAudioThread;
use crate::graph::{AudioGraph, DSEdgeID, Edge};
use crate::plugin_host::error::{ActivatePluginError, RescanParamListError};
use crate::plugin_host::{ParamModifiedInfo, PluginHostMainThread};
use crate::plugin_scanner::{PluginScanner, ScanExternalPluginsRes};
use crate::processor_schedule::TransportHandle;
use crate::utils::thread_id::SharedThreadIDs;

use super::error::{EngineCrashError, NewPluginInstanceError};
use super::modify_request::{ModifyGraphRequest, PluginIDReq};
use super::timer_wheel::{EngineTimerWheel, TimerEntry, TimerEntryKey};
use super::{DEFAULT_GARBAGE_COLLECT_INTERVAL_MS, DEFAULT_IDLE_INTERVAL_MS};

struct ActivatedState {
    audio_graph: AudioGraph,
    run_process_thread: Arc<AtomicBool>,
    process_thread_handle: Option<JoinHandle<()>>,
    tempo_map_shared: Shared<SharedCell<(Shared<TempoMap>, u64)>>,
}

impl Drop for ActivatedState {
    fn drop(&mut self) {
        // Attempt to gracefully stop the process thread.
        self.run_process_thread.store(false, Ordering::Relaxed);

        if let Some(process_thread_handle) = self.process_thread_handle.take() {
            if let Err(e) = process_thread_handle.join() {
                log::error!("Failed to join process thread handle: {:?}", e);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSettings {
    pub main_idle_interval_ms: u32,
    pub garbage_collect_interval_ms: u32,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            main_idle_interval_ms: DEFAULT_IDLE_INTERVAL_MS,
            garbage_collect_interval_ms: DEFAULT_GARBAGE_COLLECT_INTERVAL_MS,
        }
    }
}

pub struct DSEngineMainThread {
    activated_state: Option<ActivatedState>,
    timer_wheel: EngineTimerWheel,
    host_info: Shared<HostInfo>,
    plugin_scanner: PluginScanner,
    thread_ids: SharedThreadIDs,
    collector: Collector,
    crash_msg: Option<EngineCrashError>,
    cached_elapsed_entries: Option<Vec<Rc<TimerEntry>>>,
}

impl DSEngineMainThread {
    /// Construct a new Dropseed engine.
    ///
    /// * `host_info` - The information about this host.
    /// * `engine_settings` - Additional settings for the engine.
    /// * `internal_plugins` - A list of plugin factories for internal plugins.
    ///
    /// This returns:
    /// * (
    ///     * Self,
    ///     * The first timer instant when `Self::on_timer()` must be called,
    ///     * The result of scanning internal plugins
    /// * )
    pub fn new(
        host_info: HostInfo,
        settings: EngineSettings,
        mut internal_plugins: Vec<Box<dyn PluginFactory>>,
    ) -> (Self, Instant, Vec<Result<ScannedPluginKey, String>>) {
        // Set up and run garbage collector wich collects and safely drops garbage from
        // the audio thread.
        let collector = Collector::new();

        let host_info = Shared::new(&collector.handle(), host_info);

        let thread_ids =
            SharedThreadIDs::new(Some(thread::current().id()), None, &collector.handle());

        let mut plugin_scanner =
            PluginScanner::new(collector.handle(), Shared::clone(&host_info), thread_ids.clone());

        // Scan the user's internal plugins.
        let internal_plugins_res: Vec<Result<ScannedPluginKey, String>> =
            internal_plugins.drain(..).map(|p| plugin_scanner.scan_internal_plugin(p)).collect();

        let timer_wheel = EngineTimerWheel::new(
            settings.main_idle_interval_ms,
            settings.garbage_collect_interval_ms,
        );

        let next_timer_callback_instant = timer_wheel.next_expected_tick_instant();

        (
            Self {
                activated_state: None,
                timer_wheel,
                host_info,
                plugin_scanner,
                thread_ids,
                collector,
                crash_msg: None,
                cached_elapsed_entries: None,
            },
            next_timer_callback_instant,
            internal_plugins_res,
        )
    }

    /// Retrieve the info about this host
    pub fn host_info(&self) -> &HostInfo {
        &*self.host_info
    }

    // TODO: multiple transports
    /// Replace the old tempo map with this new one.
    pub fn update_tempo_map(&mut self, new_tempo_map: TempoMap) {
        if let Some(activated_state) = &mut self.activated_state {
            let tempo_map_version = activated_state.tempo_map_shared.get().1;

            let new_tempo_map_shared = Shared::new(&self.collector.handle(), new_tempo_map);

            activated_state.tempo_map_shared.set(Shared::new(
                &self.collector.handle(),
                (Shared::clone(&new_tempo_map_shared), tempo_map_version + 1),
            ));

            activated_state.audio_graph.update_tempo_map(new_tempo_map_shared);
        }
    }

    /// Get an immutable reference to the host for a particular plugin.
    ///
    /// This will return `None` if a plugin with the given ID does not exist/
    /// has been removed.
    pub fn plugin_host(&self, id: &PluginInstanceID) -> Option<&PluginHostMainThread> {
        self.activated_state.as_ref().and_then(|a| a.audio_graph.get_plugin_host(id))
    }

    /// Get a mutable reference to the host for a particular plugin.
    ///
    /// This will return `None` if a plugin with the given ID does not exist/
    /// has been removed.
    pub fn plugin_host_mut(&mut self, id: &PluginInstanceID) -> Option<&mut PluginHostMainThread> {
        self.activated_state.as_mut().and_then(|a| a.audio_graph.get_plugin_host_mut(id))
    }

    /// This must be called periodically.
    ///
    /// This will return a list of events that have occured, as well as the next
    /// instant that this method should be called again.
    pub fn on_timer(&mut self) -> (SmallVec<[OnIdleEvent; 32]>, Instant) {
        let mut events_out: SmallVec<[OnIdleEvent; 32]> = SmallVec::new();

        if let Some(msg) = self.crash_msg.take() {
            events_out.push(OnIdleEvent::EngineDeactivated(
                EngineDeactivatedStatus::EngineCrashed(Box::new(msg)),
            ));
        }

        let mut elapsed_entries =
            self.cached_elapsed_entries.take().unwrap_or_else(|| Vec::with_capacity(32));
        elapsed_entries.clear();
        self.timer_wheel.advance(&mut elapsed_entries);
        let next_timer_instant = self.timer_wheel.next_expected_tick_instant();

        for elapsed_entry in elapsed_entries.drain(..) {
            match elapsed_entry.key {
                TimerEntryKey::MainIdleTimer => {
                    if let Some(activated_state) = &mut self.activated_state {
                        let recompile = activated_state
                            .audio_graph
                            .on_idle(&mut events_out, &mut self.timer_wheel);

                        if recompile {
                            self.compile_audio_graph();
                        }
                    }
                }
                TimerEntryKey::GarbageCollectTimer => {
                    self.collect_garbage();
                }
                TimerEntryKey::PluginRegisteredTimer { plugin_unique_id, timer_id } => {
                    if let Some(activated_state) = &mut self.activated_state {
                        if let Some(plugin_host) = activated_state
                            .audio_graph
                            .get_plugin_host_by_unique_id_mut(plugin_unique_id)
                        {
                            plugin_host.on_timer(timer_id, &mut self.timer_wheel);
                        }
                    }
                }
            }
        }

        self.cached_elapsed_entries = Some(elapsed_entries);

        (events_out, next_timer_instant)
    }

    #[cfg(feature = "clap-host")]
    /// Add a new directory for scanning CLAP plugins.
    ///
    /// This returns `false` if it failed to add the directory or if that
    /// directory has already been added.
    pub fn add_clap_scan_directory<P: Into<PathBuf>>(&mut self, path: P) -> bool {
        self.plugin_scanner.add_clap_scan_directory(path.into())
    }

    #[cfg(feature = "clap-host")]
    /// Remove a directory for scanning CLAP plugins.
    ///
    /// This returns `false` if it failed to remove the directory or if that
    /// directory has already been removed.
    pub fn remove_clap_scan_directory<P: Into<PathBuf>>(&mut self, path: P) -> bool {
        self.plugin_scanner.remove_clap_scan_directory(path.into())
    }

    /// (Re)scan all external plugins.
    ///
    /// This will a return a new list of all the external plugins.
    pub fn scan_external_plugins(&mut self) -> ScanExternalPluginsRes {
        self.plugin_scanner.scan_external_plugins()
    }

    /// Activate the engine.
    ///
    /// This will return `None` if the engine is already activated.
    pub fn activate_engine(
        &mut self,
        settings: ActivateEngineSettings,
    ) -> Option<(ActivatedEngineInfo, DSEngineAudioThread)> {
        if self.activated_state.is_some() {
            log::warn!("Ignored request to activate RustyDAW engine: Engine is already activated");
            return None;
        }

        log::info!("Activating RustyDAW engine...");

        let num_audio_in_channels = settings.num_audio_in_channels;
        let num_audio_out_channels = settings.num_audio_out_channels;
        let min_frames = settings.min_frames;
        let max_frames = settings.max_frames;
        let sample_rate = settings.sample_rate;
        let note_buffer_size = settings.note_buffer_size;
        let event_buffer_size = settings.event_buffer_size;
        let transport_declick_time = settings.transport_declick_time;

        let (audio_graph, shared_schedule, transport_handle) = AudioGraph::new(
            self.collector.handle(),
            usize::from(num_audio_in_channels),
            usize::from(num_audio_out_channels),
            sample_rate,
            min_frames,
            max_frames,
            note_buffer_size,
            event_buffer_size,
            self.thread_ids.clone(),
            transport_declick_time,
            &mut self.timer_wheel,
        );

        let (audio_thread, mut process_thread) = DSEngineAudioThread::new(
            shared_schedule,
            sample_rate,
            num_audio_in_channels as usize,
            num_audio_out_channels as usize,
            max_frames as usize,
            &self.collector.handle(),
        );

        let run_process_thread = Arc::new(AtomicBool::new(true));
        let run_process_thread_clone = Arc::clone(&run_process_thread);

        let process_thread_handle =
            thread_priority::spawn(ThreadPriority::Max, move |priority_res| {
                if let Err(e) = priority_res {
                    log::error!("Failed to set process thread priority to max: {:?}", e);
                } else {
                    log::info!("Successfully set process thread priority to max");
                }

                process_thread.run(run_process_thread_clone);
            });

        let tempo_map_shared = transport_handle.tempo_map_shared();
        let tempo_map = (*tempo_map_shared.get().0).clone();

        let info = ActivatedEngineInfo {
            graph_in_id: audio_graph.graph_in_id().clone(),
            graph_out_id: audio_graph.graph_out_id().clone(),
            sample_rate,
            min_frames,
            max_frames,
            transport_handle,
            num_audio_in_channels,
            num_audio_out_channels,
            tempo_map,
        };

        self.activated_state = Some(ActivatedState {
            audio_graph,
            run_process_thread,
            process_thread_handle: Some(process_thread_handle),
            tempo_map_shared,
        });

        self.compile_audio_graph();

        if self.activated_state.is_none() {
            panic!("Unexpected error: Empty audio graph failed to compile a schedule.");
        }

        log::info!("Successfully activated RustyDAW engine");

        Some((info, audio_thread))
    }

    /// Modify the audio graph.
    ///
    /// This will return `None` if the engine is deactivated.
    pub fn modify_graph(&mut self, mut request: ModifyGraphRequest) -> Option<ModifyGraphRes> {
        if let Some(activated_state) = &mut self.activated_state {
            let mut removed_edges: FnvHashSet<DSEdgeID> = FnvHashSet::default();
            let mut new_edges: Vec<Edge> = Vec::new();

            for ds_edge_id in request.disconnect_edges.iter() {
                if activated_state.audio_graph.disconnect_edge(*ds_edge_id) {
                    removed_edges.insert(*ds_edge_id);
                }
            }

            let (mut removed_plugins, removed_edges) = activated_state
                .audio_graph
                .remove_plugin_instances(&request.remove_plugin_instances, &mut self.timer_wheel);

            let new_plugins_res: Vec<NewPluginRes> = request
                .add_plugin_instances
                .drain(..)
                .map(|save_state| {
                    activated_state.audio_graph.add_new_plugin_instance(
                        save_state,
                        &mut self.plugin_scanner,
                        true,
                    )
                })
                .collect();

            let new_plugin_ids: Vec<PluginInstanceID> =
                new_plugins_res.iter().map(|res| res.plugin_id.clone()).collect();

            for edge in request.connect_new_edges.iter() {
                let src_plugin_id = match &edge.src_plugin_id {
                    PluginIDReq::Added(index) => {
                        if let Some(new_plugin_id) = new_plugin_ids.get(*index) {
                            new_plugin_id
                        } else {
                            log::error!(
                                "Could not connect edge {:?}: Source plugin index out of bounds",
                                edge
                            );
                            continue;
                        }
                    }
                    PluginIDReq::Existing(id) => id,
                };

                let dst_plugin_id = match &edge.dst_plugin_id {
                    PluginIDReq::Added(index) => {
                        if let Some(new_plugin_id) = new_plugin_ids.get(*index) {
                            new_plugin_id
                        } else {
                            log::error!(
                                "Could not connect edge {:?}: Destination plugin index out of bounds",
                                edge
                            );
                            continue;
                        }
                    }
                    PluginIDReq::Existing(id) => id,
                };

                match activated_state.audio_graph.connect_edge(edge, src_plugin_id, dst_plugin_id) {
                    Ok(new_edge) => new_edges.push(new_edge),
                    Err(e) => {
                        if edge.log_error_on_fail {
                            log::warn!("Could not connect edge: {}", e);
                        } else {
                            #[cfg(debug_assertions)]
                            log::debug!("Could not connect edge: {}", e);
                        }
                    }
                }
            }

            let res = ModifyGraphRes {
                new_plugins: new_plugins_res,
                removed_plugins: removed_plugins.drain().collect(),
                new_edges,
                removed_edges,
            };

            // TODO: Compile audio graph in a separate thread?
            self.compile_audio_graph();

            Some(res)
        } else {
            log::warn!("Cannot modify audio graph: Engine is deactivated");
            None
        }
    }

    /// Gracefully deactivate the engine. This will also reset the audio
    /// graph and remove all plugins.
    ///
    /// Make sure that the engine is deactivated or dropped in the main
    /// thread before exiting your program.
    ///
    /// This will return `false` if the engine is already deactivated.
    pub fn deactivate_engine(&mut self) -> bool {
        if self.activated_state.is_none() {
            log::warn!("Ignored request to deactivate engine: Engine is already deactivated");
            return false;
        }

        log::info!("Deactivating RustyDAW engine");

        let mut activated_state = self.activated_state.take().unwrap();

        // Attempt to remove all plugins gracefully.
        activated_state.audio_graph.reset(&mut self.timer_wheel);

        // Attempt to stop the process thread gracefully.
        let _ = activated_state;

        self.timer_wheel.reset();

        self.crash_msg = None;

        self.collect_garbage();

        true
    }

    /// Returns `true` if the engine is currently activated.
    pub fn is_activated(&self) -> bool {
        self.activated_state.is_some()
    }

    /// Collect the latest save states for all plugins.
    ///
    /// This will only return the save states of plugins which have
    /// changed since the last call to collect its save state.
    pub fn collect_latest_save_states(&mut self) -> Vec<(PluginInstanceID, DSPluginSaveState)> {
        if self.activated_state.is_none() {
            log::warn!("Ignored request for the latest save states: Engine is deactivated");
            return Vec::new();
        }

        log::trace!("Got request for latest plugin save states");

        self.activated_state.as_mut().unwrap().audio_graph.collect_save_states()
    }

    fn collect_garbage(&mut self) {
        self.plugin_scanner.unload_unused_binaries();
        self.collector.collect();
    }

    fn compile_audio_graph(&mut self) {
        if let Some(mut activated_state) = self.activated_state.take() {
            match activated_state.audio_graph.compile() {
                Ok(_) => {
                    self.activated_state = Some(activated_state);
                }
                Err(e) => {
                    log::error!("{}", e);

                    // Audio graph is in an invalid state. Drop it and have the user restore
                    // from the last working save state.

                    // Attempt to remove all plugins gracefully.
                    activated_state.audio_graph.reset(&mut self.timer_wheel);

                    // Attempt to stop the process thread gracefully.
                    let _ = activated_state;

                    self.timer_wheel.reset();

                    self.crash_msg = Some(EngineCrashError::CompilerError(e));
                }
            }
        }
    }
}

impl Drop for DSEngineMainThread {
    fn drop(&mut self) {
        if self.activated_state.is_some() {
            self.deactivate_engine();
        } else {
            self.collect_garbage();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ActivateEngineSettings {
    /// The sample rate of the project.
    pub sample_rate: SampleRate,

    /// The minimum number of frames (samples in a single audio channel)
    /// the can be processed in a single process cycle.
    pub min_frames: u32,

    /// The maximum number of frames (samples in a single audio channel)
    /// the can be processed in a single process cycle.
    pub max_frames: u32,

    /// The total number of input audio channels to the audio graph.
    pub num_audio_in_channels: u16,

    /// The total number of output audio channels from the audio graph.
    pub num_audio_out_channels: u16,

    /// The pre-allocated capacity for note buffers in the audio graph.
    ///
    /// By default this is set to `256`.
    pub note_buffer_size: usize,

    /// The pre-allocated capacity for parameter event buffers in the audio
    /// graph.
    ///
    /// By default this is set to `256`.
    pub event_buffer_size: usize,

    /// The time window for the transport's declick buffers.
    ///
    /// Set this to `None` to have no transport declicking.
    ///
    /// By default this is set to `None`.
    pub transport_declick_time: Option<Seconds>,
}

impl Default for ActivateEngineSettings {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate::default(),
            min_frames: 1,
            max_frames: 512,
            num_audio_in_channels: 2,
            num_audio_out_channels: 2,
            note_buffer_size: 256,
            event_buffer_size: 256,
            transport_declick_time: None,
        }
    }
}

pub struct ActivatedEngineInfo {
    /// The ID for the input to the audio graph. Use this to connect any
    /// plugins to system inputs.
    pub graph_in_id: PluginInstanceID,

    /// The ID for the output to the audio graph. Use this to connect any
    /// plugins to system outputs.
    pub graph_out_id: PluginInstanceID,

    /// The handle to the tranport.
    pub transport_handle: TransportHandle,

    /// The current tempo map of the transport.
    pub tempo_map: TempoMap,

    /// The sample rate of the project.
    pub sample_rate: SampleRate,

    /// The minimum number of frames (samples in a single audio channel)
    /// the can be processed in a single process cycle.
    pub min_frames: u32,

    /// The maximum number of frames (samples in a single audio channel)
    /// the can be processed in a single process cycle.
    pub max_frames: u32,

    /// The total number of input audio channels to the audio graph.
    pub num_audio_in_channels: u16,

    /// The total number of output audio channels from the audio graph.
    pub num_audio_out_channels: u16,
}

impl std::fmt::Debug for ActivatedEngineInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("ActivatedEngineInfo");

        f.field("graph_in_id", &self.graph_in_id);
        f.field("graph_out_id", &self.graph_out_id);
        f.field("sample_rate", &self.sample_rate);
        f.field("min_frames", &self.min_frames);
        f.field("max_frames", &self.max_frames);
        f.field("num_audio_in_channels", &self.num_audio_in_channels);
        f.field("num_audio_out_channels", &self.num_audio_out_channels);

        f.finish()
    }
}

#[derive(Debug)]
/// Sent whenever the engine has become deactivated, whether gracefully
/// or because of a crash.
pub enum EngineDeactivatedStatus {
    /// The engine was deactivated gracefully.
    DeactivatedGracefully,
    /// The engine has crashed.
    EngineCrashed(Box<EngineCrashError>),
}

#[derive(Debug)]
pub struct PluginActivatedStatus {
    /// Returns `true` if the plugin has updated its list of audio ports.
    ///
    /// If `true`, then use `PluginHostMainThread::audio_ports_ext()` to
    /// retrieve the new list of audio ports.
    pub has_new_audio_ports_ext: bool,

    /// Returns `true` if the plugin has updated its list of note ports.
    ///
    /// If `true`, then use `PluginHostMainThread::note_ports_ext()` to
    /// retrieve the new list of note ports.
    pub has_new_note_ports_ext: bool,

    /// Returns `true` if the plugin has changed its latency.
    ///
    /// If `true`, then use `PluginHostMainThread::latency()` to
    /// retrieve the new latency.
    pub has_new_latency: bool,

    /// If this is an internal plugin with a custom defined handle,
    /// then this will be the new custom handle.
    pub internal_handle: Option<Box<dyn std::any::Any + Send + 'static>>,

    /// Any edges that were removed as a result of the plugin removing
    /// some of its ports.
    pub removed_edges: Vec<DSEdgeID>,

    /// This is `true` if activating this plugin caused the audio graph
    /// to recompile.
    pub caused_recompile: bool,
}

#[derive(Debug)]
pub enum PluginStatus {
    /// This means the plugin successfully activated and returned
    /// its new configurations.
    Activated(PluginActivatedStatus),

    /// This means that the plugin loaded but did not activate yet. This
    /// can happen when the user loads a project with a deactivated
    /// plugin.
    Inactive,

    /// There was an error loading the plugin.
    LoadError(NewPluginInstanceError),

    /// There was an error activating the plugin.
    ActivationError(ActivatePluginError),
}

#[derive(Debug)]
pub struct NewPluginRes {
    /// The unique ID for this plugin instance.
    pub plugin_id: PluginInstanceID,

    /// The status of this plugin.
    pub status: PluginStatus,

    /// Whether or not this plugin instance supports creating a custom
    /// GUI in a floating window that the plugin manages itself.
    pub supports_floating_gui: bool,

    /// Whether or not this plugin instance supports embedding a custom
    /// GUI into a window managed by the host.
    pub supports_embedded_gui: bool,
}

#[derive(Debug)]
pub struct ModifyGraphRes {
    /// Any new plugins that were added to the graph.
    pub new_plugins: Vec<NewPluginRes>,

    /// Any plugins that were successfully removed from the graph.
    pub removed_plugins: Vec<PluginInstanceID>,

    /// All of the edges (port connections) that have been successfully
    /// connected as a result of this operation.
    pub new_edges: Vec<Edge>,

    /// All of the edges (port connections) that have been removed as
    /// a result of this operation.
    pub removed_edges: Vec<DSEdgeID>,
}

#[derive(Debug)]
pub enum OnIdleEvent {
    /// The plugin's parameters have been modified via the plugin's custom
    /// GUI.
    ///
    /// Only the parameters which have changed will be returned in this
    /// field.
    PluginParamsModified {
        plugin_id: PluginInstanceID,
        modified_params: SmallVec<[ParamModifiedInfo; 4]>,
    },

    /// The plugin requested the app to resize its gui to the given size.
    ///
    /// This event will only be sent if using an embedded window for the
    /// plugin GUI.
    PluginRequestedToResizeGui { plugin_id: PluginInstanceID, size: GuiSize },

    /// The plugin requested the app to show its GUI.
    ///
    /// This event will only be sent if using an embedded window for the
    /// plugin GUI.
    PluginRequestedToShowGui { plugin_id: PluginInstanceID },

    /// The plugin requested the app to hide its GUI.
    ///
    /// Note that hiding the GUI is not the same as destroying the GUI.
    /// Hiding only hides the window content, it does not free the GUI's
    /// resources.  Yet it may be a good idea to stop painting timers
    /// when a plugin GUI is hidden.
    ///
    /// This event will only be sent if using an embedded window for the
    /// plugin GUI.
    PluginRequestedToHideGui { plugin_id: PluginInstanceID },

    /// Sent when the plugin closed its own GUI by its own means. UI should
    /// be updated accordingly so that the user could open the UI again.
    ///
    /// If `was_destroyed` is `true`, then the app must call
    /// `PluginHostMainThread::destroy_gui()` to acknowledge the gui
    /// destruction.
    PluginGuiClosed { plugin_id: PluginInstanceID, was_destroyed: bool },

    /// Sent when the plugin changed the resize hint information on how
    /// to resize its GUI.
    ///
    /// This event will only be sent if using an embedded window for the
    /// plugin GUI.
    PluginChangedGuiResizeHints {
        plugin_id: PluginInstanceID,
        resize_hints: Option<GuiResizeHints>,
    },

    /// The plugin has updated its list of parameters.
    PluginUpdatedParameterList {
        plugin_id: PluginInstanceID,
        status: Result<(), RescanParamListError>,
    },

    /// Sent whenever a plugin becomes activated after being deactivated or
    /// when the plugin restarts.
    ///
    /// Make sure your UI updates the port configuration on this plugin, as
    /// well as any custom handles.
    PluginActivated { plugin_id: PluginInstanceID, status: PluginActivatedStatus },

    /// Sent whenever a plugin has been deactivated.
    PluginDeactivated {
        plugin_id: PluginInstanceID,
        /// If this is `Ok(())`, then it means the plugin was gracefully
        /// deactivated via user request.
        ///
        /// If this is `Err(e)`, then it means the plugin became deactivated
        /// because it failed to restart.
        status: Result<(), ActivatePluginError>,
    },

    /// Sent whenever the engine has been deactivated, whether gracefully or
    /// because of a crash.
    EngineDeactivated(EngineDeactivatedStatus),
}
