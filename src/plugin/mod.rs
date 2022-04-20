use std::error::Error;
use std::path::PathBuf;

use basedrop::Shared;

use crate::host::{Host, HostInfo};
use crate::AudioPortBuffer;

pub mod ext;

pub(crate) mod process_info;

mod save_state;

use process_info::{ProcInfo, ProcessStatus};

pub use save_state::PluginSaveState;

/// The description of a plugin.
#[derive(Debug, Clone)]
pub struct PluginDescriptor {
    /// The unique reverse-domain-name identifier of this plugin.
    ///
    /// eg: "org.rustydaw.spicysynth"
    pub id: String,

    /// The displayable name of this plugin.
    ///
    /// eg: "Spicy Synth"
    pub name: String,

    /// The vendor of this plugin.
    ///
    /// eg: "RustyDAW"
    pub vendor: String,

    /// The version of this plugin.
    ///
    /// eg: "1.4.4" or "1.1.2_beta"
    pub version: String,

    /// A displayable short description of this plugin.
    ///
    /// eg: "Create flaming-hot sounds!"
    pub description: String,

    /// Arbitrary list of keywords, separated by `;'.
    ///
    /// They can be matched by the host search engine and used to classify the plugin.
    ///
    /// Some pre-defined keywords:
    /// - "instrument", "audio_effect", "note_effect", "analyzer"
    /// - "mono", "stereo", "surround", "ambisonic"
    /// - "distortion", "compressor", "limiter", "transient"
    /// - "equalizer", "filter", "de-esser"
    /// - "delay", "reverb", "chorus", "flanger"
    /// - "tool", "utility", "glitch"
    ///
    /// Some examples:
    /// - "equalizer;analyzer;stereo;mono"
    /// - "compressor;analog;character;mono"
    /// - "reverb;plate;stereo"
    pub features: Option<String>,

    /// The url to the product page of this plugin.
    ///
    /// Set to `None` if there is no product page.
    pub url: Option<String>,

    /// The url to the online manual for this plugin.
    ///
    /// Set to `None` if there is no online manual.
    pub manual_url: Option<String>,

    /// The url to the online support page for this plugin.
    ///
    /// Set to `None` if there is no online support page.
    pub support_url: Option<String>,
}

/// The methods of an audio plugin which are used to create new instances of the plugin.
pub trait PluginFactory {
    /// This function is always called first and only once.
    ///
    /// * `plugin_path` - The path to the shared library that was loaded. This will be `None`
    /// for internal plugins.
    ///
    /// This method should be as fast as possible, in order to perform very quick scan of the plugin
    /// descriptors.
    ///
    /// It is forbidden to display graphical user interface in this call.
    /// It is forbidden to perform user inter-action in this call.
    ///
    /// If the initialization depends upon expensive computation, maybe try to do them ahead of time
    /// and cache the result.
    #[allow(unused_attributes)]
    fn entry_init(
        &mut self,
        plugin_path: Option<&PathBuf>,
    ) -> Result<PluginDescriptor, Box<dyn Error>>;

    /// Create a new instance of this plugin.
    ///
    /// A `basedrop` collector handle is provided for realtime-safe garbage collection.
    ///
    /// `[main-thread]`
    fn new(
        &mut self,
        host_info: Shared<HostInfo>,
        coll_handle: &basedrop::Handle,
    ) -> Result<Box<dyn PluginMainThread>, Box<dyn Error>>;
}

/// The methods of an audio plugin instance which run in the "main" thread.
pub trait PluginMainThread {
    /// This is called after creating a plugin instance and once it's safe for the plugin to
    /// use the host callback methods.
    ///
    /// A `basedrop` collector handle is provided for realtime-safe garbage collection.
    ///
    /// By default this does nothing.
    ///
    /// `[main-thread & !active_state]`
    #[allow(unused)]
    fn init(&mut self, host: &Host, coll_handle: &basedrop::Handle) {}

    /// Activate the plugin, and return the `PluginAudioThread` counterpart.
    ///
    /// In this call the plugin may allocate memory and prepare everything needed for the process
    /// call. The process's sample rate will be constant and process's frame count will included in
    /// the `[min, max]` range, which is bounded by `[1, INT32_MAX]`.
    ///
    /// A `basedrop` collector handle is provided for realtime-safe garbage collection.
    ///
    /// Once activated the latency and port configuration must remain constant, until deactivation.
    ///
    /// `[main-thread & !active_state]`
    fn activate(
        &mut self,
        sample_rate: f64,
        min_frames: usize,
        max_frames: usize,
        host: &Host,
        coll_handle: &basedrop::Handle,
    ) -> Result<Box<dyn PluginAudioThread>, Box<dyn Error>>;

    /// Deactivate the plugin. When this is called it also means that the `PluginAudioThread`
    /// counterpart has/will be dropped.
    ///
    /// `[main-thread & active_state]`
    fn deactivate(&mut self, host: &Host);

    /// Called by the host on the main thread in response to a previous call to `host.request_callback()`.
    ///
    /// By default this does nothing.
    ///
    /// [main-thread]
    #[allow(unused)]
    fn on_main_thread(&mut self, host: &Host) {}

    /// An optional extension that describes the configuration of audio ports on this plugin instance.
    ///
    /// This will only be called while the plugin is inactive.
    ///
    /// The default configuration is a main stereo input port and a main stereo output port.
    ///
    /// [main-thread & !active_state]
    #[allow(unused)]
    fn audio_ports_extension(&self, host: &Host) -> ext::audio_ports::AudioPortsExtension {
        ext::audio_ports::AudioPortsExtension::default()
    }
}

/// The methods of an audio plugin instance which run in the "audio" thread.
pub trait PluginAudioThread: Send + 'static {
    /// This will be called each time before a call to `process()`.
    ///
    /// Return an error if the plugin failed to start processing. In this case the host will not
    /// call `process()` this process cycle.
    ///
    /// By default this just returns `Ok(())`.
    ///
    /// `[audio-thread & active_state & !processing_state]`
    #[allow(unused)]
    fn start_processing(&mut self, host: &Host) -> Result<(), ()> {
        Ok(())
    }

    /// This will be called each time after a call to `process()`.
    ///
    /// By default this does nothing.
    ///
    /// `[audio-thread & active_state & processing_state]`
    #[allow(unused)]
    fn stop_processing(&mut self, host: &Host) {}

    /// Process audio and events.
    ///
    /// `[audio-thread & active_state & processing_state]`
    fn process(
        &mut self,
        info: &ProcInfo,
        audio_in: &[AudioPortBuffer],
        audio_out: &mut [AudioPortBuffer],
        host: &Host,
    ) -> ProcessStatus;
}
