pub mod automation;
pub mod buffer;
pub mod ext;
pub mod host_request_channel;
pub mod plugin_scanner;
pub mod transport;

mod descriptor;
mod factory;
mod host_info;
mod instance_id;
mod main_thread;
mod process_info;
mod processor;
mod save_state;

pub use buffer::{AudioPortBuffer, AudioPortBufferMut};
pub use descriptor::PluginDescriptor;
pub use ext::params::ParamID;
pub use factory::PluginFactory;
pub use host_info::HostInfo;
pub use host_request_channel::*;
pub use instance_id::*;
pub use main_thread::{PluginActivatedInfo, PluginMainThread};
pub use process_info::{ProcBuffers, ProcInfo, ProcessStatus};
pub use processor::PluginProcessor;
pub use save_state::DSPluginSaveState;

pub use clack_host::events::event_types as event;
pub use clack_host::utils::FixedPoint;
