use smallvec::SmallVec;

use dropseed_plugin_api::automation::AutomationIoEvent;
use dropseed_plugin_api::buffer::SharedBuffer;

pub(crate) struct AutomationSumTask {
    pub input: SmallVec<[SharedBuffer<AutomationIoEvent>; 4]>,
    pub output: SharedBuffer<AutomationIoEvent>,
}

impl AutomationSumTask {
    pub fn process(&mut self) {
        let mut out_buf = self.output.borrow_mut();
        out_buf.clear();

        for in_buf in self.input.iter() {
            let in_buf = in_buf.borrow();
            out_buf.extend_from_slice(in_buf.as_slice());
        }

        // TODO: Sanitize buffers with `PluginEventOutputSanitizer`?
    }
}
