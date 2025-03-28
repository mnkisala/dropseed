use smallvec::SmallVec;

use dropseed_plugin_api::buffer::SharedBuffer;
use dropseed_plugin_api::ProcInfo;

pub(crate) struct AudioSumTask {
    pub audio_in: SmallVec<[SharedBuffer<f32>; 4]>,
    pub audio_out: SharedBuffer<f32>,
}

impl AudioSumTask {
    pub fn process(&mut self, proc_info: &ProcInfo) {
        let mut out_ref = self.audio_out.borrow_mut();

        let out = &mut out_ref[0..proc_info.frames];

        let in_0_ref = self.audio_in[0].borrow();
        let in_0 = &in_0_ref[0..proc_info.frames];

        let mut is_constant = self.audio_in[0].is_constant();

        out.copy_from_slice(in_0);

        for ch in self.audio_in.iter().skip(1) {
            let input_ref = ch.borrow();

            let input = &input_ref[0..proc_info.frames];

            if ch.is_constant() {
                if input[0].abs() <= std::f32::EPSILON {
                    // We can skip this one since it is silent.
                    continue;
                } else {
                    let val = input[0];
                    for smp in out.iter_mut() {
                        *smp += val;
                    }
                }
            } else {
                is_constant = false;

                for smp_i in 0..proc_info.frames {
                    out[smp_i] += input[smp_i];
                }
            }
        }

        self.audio_out.set_constant(is_constant);
    }
}
