use smallvec::SmallVec;

use dropseed_plugin_api::buffer::SharedBuffer;

use crate::plugin_host::event_io_buffers::NoteIoEvent;

pub(crate) struct NoteSumTask {
    pub note_in: SmallVec<[SharedBuffer<NoteIoEvent>; 4]>,
    pub note_out: SharedBuffer<NoteIoEvent>,
}

impl NoteSumTask {
    pub fn process(&mut self) {
        let mut out_buf = self.note_out.borrow_mut();
        out_buf.clear();

        for in_buf in self.note_in.iter() {
            let in_buf = in_buf.borrow();
            out_buf.extend_from_slice(in_buf.as_slice());
        }
    }
}
