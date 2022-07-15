//mod clap_plugin_host;

mod engine;
mod graph;

#[cfg(feature = "clap-host")]
mod clap;

pub mod transport;
pub mod utils;

pub use clack_host::events::io::EventBuffer;
pub use clack_host::utils::FixedPoint;

pub use dropseed_core::*;

#[cfg(feature = "resource-loader")]
pub use dropseed_resource_loader as resource_loader;

pub use engine::audio_thread::DSEngineAudioThread;
pub use engine::events::from_engine::{
    DSEngineEvent, EngineDeactivatedInfo, PluginEvent, PluginScannerEvent,
};
pub use engine::events::to_engine::DSEngineRequest;
pub use engine::handle::DSEngineHandle;
pub use engine::main_thread::{
    ActivateEngineSettings, EdgeReq, EdgeReqPortID, EngineActivatedInfo, ModifyGraphRequest,
    ModifyGraphRes, PluginIDReq,
};
pub use engine::plugin_scanner::{RescanPluginDirectoriesRes, ScannedPlugin};
pub use graph::{
    ActivatePluginError, AudioGraphSaveState, Edge, NewPluginRes, ParamGestureInfo,
    ParamModifiedInfo, PluginActivationStatus, PluginEdges, PluginHandle, PluginParamsExt,
    PortType,
};
