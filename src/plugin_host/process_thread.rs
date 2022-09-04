use clack_host::events::Event;
use dropseed_plugin_api::buffer::EventBuffer;
use dropseed_plugin_api::{PluginProcessThread, ProcBuffers, ProcInfo, ProcessStatus};

use crate::utils::thread_id::SharedThreadIDs;

use super::channel::{PlugHostChannelProcThread, PluginActiveState};
use super::event_io_buffers::{PluginEventIoBuffers, PluginEventOutputSanitizer};

#[derive(Copy, Clone, Debug, PartialEq)]
enum ProcessingState {
    WaitingForStart,
    Started(ProcessStatus),
    Stopped,
    Errored,
}

pub(crate) struct PluginHostProcessor {
    plugin: Box<dyn PluginProcessThread>,
    plugin_instance_id: u64,

    channel: PlugHostChannelProcThread,

    in_events: EventBuffer,
    out_events: EventBuffer,

    event_output_sanitizer: PluginEventOutputSanitizer,

    processing_state: ProcessingState,

    thread_ids: SharedThreadIDs,

    schedule_version: u64,

    bypassed: bool,
    bypass_declick: f32,
    bypass_declick_inc: f32,
    bypass_declick_frames: usize,
    bypass_declick_frames_left: usize,
}

impl PluginHostProcessor {
    pub(crate) fn new(
        plugin: Box<dyn PluginProcessThread>,
        plugin_instance_id: u64,
        channel: PlugHostChannelProcThread,
        num_params: usize,
        thread_ids: SharedThreadIDs,
        schedule_version: u64,
        bypass_declick_frames: usize,
    ) -> Self {
        debug_assert_ne!(bypass_declick_frames, 0);

        let bypassed = channel.shared_state.bypassed();
        let bypass_declick = if bypassed { 1.0 } else { 0.0 };
        let bypass_declick_inc = 1.0 / bypass_declick_frames as f32;

        Self {
            plugin,
            plugin_instance_id,
            channel,
            in_events: EventBuffer::with_capacity(num_params * 3),
            out_events: EventBuffer::with_capacity(num_params * 3),
            event_output_sanitizer: PluginEventOutputSanitizer::new(num_params),
            processing_state: ProcessingState::WaitingForStart,
            thread_ids,
            schedule_version,
            bypassed,
            bypass_declick,
            bypass_declick_inc,
            bypass_declick_frames,
            bypass_declick_frames_left: 0,
        }
    }

    /// Returns `true` if gotten a request to drop the processor.
    pub fn process(
        &mut self,
        proc_info: &ProcInfo,
        buffers: &mut ProcBuffers,
        event_buffers: &mut PluginEventIoBuffers,
    ) -> bool {
        // Always clear event and note output buffers.
        event_buffers.clear_before_process();

        let state = self.channel.shared_state.get_active_state();

        // Do we want to deactivate the plugin?
        if state == PluginActiveState::WaitingToDrop {
            if let ProcessingState::Started(_) = self.processing_state {
                self.plugin.stop_processing();
            }

            buffers.clear_all_outputs(proc_info);
            return true;
        } else if self.schedule_version > proc_info.schedule_version {
            // Don't process until the expected schedule arrives. This can happen
            // when a plugin restarting causes the graph to recompile, and that
            // new schedule has not yet arrived.
            buffers.clear_all_outputs(proc_info);
            return false;
        } else if self.channel.shared_state.process_requested() {
            if let ProcessingState::Started(_) = self.processing_state {
            } else {
                self.processing_state = ProcessingState::WaitingForStart;
            }
        }

        // We can't process a plugin which failed to start processing.
        if self.processing_state == ProcessingState::Errored {
            buffers.clear_all_outputs(proc_info);
            return false;
        }

        // Reading in_events from all sources //

        self.in_events.clear();
        let mut has_param_in_event = self
            .channel
            .param_queues
            .as_mut()
            .map(|q| q.consume_into_event_buffer(&mut self.in_events))
            .unwrap_or(false);

        let (has_note_in_event, wrote_param_in_event) =
            event_buffers.write_input_events(&mut self.in_events, self.plugin_instance_id);

        has_param_in_event = has_param_in_event || wrote_param_in_event;

        if let Some(transport_in_event) = proc_info.transport.event() {
            self.in_events.push(transport_in_event.as_unknown());
        }

        // Check if inputs are quiet or not //

        if self.processing_state == ProcessingState::Started(ProcessStatus::ContinueIfNotQuiet)
            && !has_note_in_event
            && buffers.audio_inputs_silent(proc_info.frames)
        {
            self.plugin.stop_processing();

            self.processing_state = ProcessingState::Stopped;
            buffers.clear_all_outputs(proc_info);

            if has_param_in_event {
                self.plugin.param_flush(&self.in_events, &mut self.out_events);
            }

            self.in_events.clear();
            return false;
        }

        // Check if the plugin should be waking up //

        if let ProcessingState::Stopped | ProcessingState::WaitingForStart = self.processing_state {
            if self.processing_state == ProcessingState::Stopped && !has_note_in_event {
                // The plugin is sleeping, there is no request to wake it up, and there
                // are no events to process.
                buffers.clear_all_outputs(proc_info);

                if has_param_in_event {
                    self.plugin.param_flush(&self.in_events, &mut self.out_events);
                }

                self.in_events.clear();
                return false;
            }

            if let Err(e) = self.plugin.start_processing() {
                log::error!("Plugin has failed to start processing: {}", e);

                // The plugin failed to start processing.
                self.processing_state = ProcessingState::Errored;
                buffers.clear_all_outputs(proc_info);

                if has_param_in_event {
                    self.plugin.param_flush(&self.in_events, &mut self.out_events);
                }

                return false;
            }

            self.channel.shared_state.set_active_state(PluginActiveState::Active);
        }

        // Actual processing //

        self.out_events.clear();

        let new_status =
            if let Some(automation_out_buffer) = &mut event_buffers.automation_out_buffer {
                let automation_out_buffer = &mut *automation_out_buffer.borrow_mut();

                self.plugin.process_with_automation_out(
                    proc_info,
                    buffers,
                    &self.in_events,
                    &mut self.out_events,
                    automation_out_buffer,
                )
            } else {
                self.plugin.process(proc_info, buffers, &self.in_events, &mut self.out_events)
            };

        // Read from output events queue //

        if let Some(params_queue) = &mut self.channel.param_queues {
            params_queue.to_main_param_value_tx.produce(|mut producer| {
                event_buffers.read_output_events(
                    &self.out_events,
                    Some(&mut producer),
                    &mut self.event_output_sanitizer,
                    proc_info.frames as u32,
                )
            });
        } else {
            event_buffers.read_output_events(
                &self.out_events,
                None,
                &mut self.event_output_sanitizer,
                proc_info.frames as u32,
            );
        }

        // Update processing state //

        self.processing_state = match new_status {
            // ProcessStatus::Tail => TODO: handle tail by reading from the tail extension
            ProcessStatus::Sleep => {
                self.plugin.stop_processing();

                ProcessingState::Stopped
            }
            ProcessStatus::Error => {
                // Discard all output buffers.
                buffers.clear_all_outputs(proc_info);
                ProcessingState::Errored
            }
            good_status => ProcessingState::Started(good_status),
        };

        // Process bypassing //

        if self.bypassed != self.channel.shared_state.bypassed() {
            self.bypassed = self.channel.shared_state.bypassed();

            if self.bypass_declick_frames_left == 0 {
                self.bypass_declick_frames_left = self.bypass_declick_frames;
                if self.bypassed {
                    self.bypass_declick = 1.0;
                } else {
                    self.bypass_declick = 0.0;
                }
            } else {
                self.bypass_declick_frames_left =
                    self.bypass_declick_frames - self.bypass_declick_frames_left;
            }
        }

        if self.bypass_declick_frames_left != 0 {
            self.bypass_declick(proc_info, buffers);
        } else if self.bypassed {
            self.bypass(proc_info, buffers);
        }

        false
    }

    fn bypass_declick(&mut self, proc_info: &ProcInfo, buffers: &mut ProcBuffers) {
        let declick_frames = self.bypass_declick_frames_left.min(proc_info.frames);

        let skip_ports = if buffers._main_audio_through_when_bypassed() {
            let main_in_port = &buffers.audio_in[0];
            let main_out_port = &mut buffers.audio_out[0];

            let in_port_iter = main_in_port._iter_raw_f32().unwrap();
            let out_port_iter = main_out_port._iter_raw_f32_mut().unwrap();

            for (in_channel, out_channel) in in_port_iter.zip(out_port_iter) {
                let in_channel_data = in_channel.borrow();
                let mut out_channel_data = out_channel.borrow_mut();
                let mut declick = self.bypass_declick;

                if self.bypassed {
                    for i in 0..declick_frames {
                        declick -= self.bypass_declick_inc;

                        out_channel_data[i] = (out_channel_data[i] * declick)
                            + (in_channel_data[i] * (1.0 - declick));
                    }
                    if declick_frames < proc_info.frames {
                        out_channel_data[declick_frames..proc_info.frames]
                            .copy_from_slice(&in_channel_data[declick_frames..proc_info.frames]);
                    }
                } else {
                    for i in 0..declick_frames {
                        declick += self.bypass_declick_inc;

                        out_channel_data[i] = (out_channel_data[i] * declick)
                            + (in_channel_data[i] * (1.0 - declick));
                    }
                }

                out_channel.set_constant(false);
            }

            for out_channel in
                main_out_port._iter_raw_f32_mut().unwrap().skip(main_in_port.channels())
            {
                let mut out_channel_data = out_channel.borrow_mut();
                let mut declick = self.bypass_declick;

                if self.bypassed {
                    for i in 0..declick_frames {
                        declick -= self.bypass_declick_inc;

                        out_channel_data[i] = out_channel_data[i] * declick;
                    }
                    if declick_frames < proc_info.frames {
                        out_channel_data[declick_frames..proc_info.frames].fill(0.0);
                    }
                } else {
                    for i in 0..declick_frames {
                        declick += self.bypass_declick_inc;

                        out_channel_data[i] = out_channel_data[i] * declick;
                    }
                }

                out_channel.set_constant(false);
            }

            1
        } else {
            0
        };

        for out_port in buffers.audio_out.iter_mut().skip(skip_ports) {
            for out_channel in out_port._iter_raw_f32_mut().unwrap() {
                let mut out_channel_data = out_channel.borrow_mut();
                let mut declick = self.bypass_declick;

                if self.bypassed {
                    for i in 0..declick_frames {
                        declick -= self.bypass_declick_inc;

                        out_channel_data[i] = out_channel_data[i] * declick;
                    }
                    if declick_frames < proc_info.frames {
                        out_channel_data[declick_frames..proc_info.frames].fill(0.0);
                    }
                } else {
                    for i in 0..declick_frames {
                        declick += self.bypass_declick_inc;

                        out_channel_data[i] = out_channel_data[i] * declick;
                    }
                }

                out_channel.set_constant(false);
            }
        }

        self.bypass_declick_frames_left -= declick_frames;
        if self.bypassed {
            self.bypass_declick -= self.bypass_declick_inc * declick_frames as f32;
        } else {
            self.bypass_declick += self.bypass_declick_inc * declick_frames as f32;
        }
    }

    fn bypass(&mut self, proc_info: &ProcInfo, buffers: &mut ProcBuffers) {
        buffers.clear_all_outputs(proc_info);

        if buffers._main_audio_through_when_bypassed() {
            let main_in_port = &buffers.audio_in[0];
            let main_out_port = &mut buffers.audio_out[0];

            if !main_in_port.has_silent_hint() {
                let in_port_iter = main_in_port._iter_raw_f32().unwrap();
                let out_port_iter = main_out_port._iter_raw_f32_mut().unwrap();

                for (in_channel, out_channel) in in_port_iter.zip(out_port_iter) {
                    let in_channel_data = out_channel.borrow();
                    let mut out_channel_data = out_channel.borrow_mut();

                    out_channel_data[0..proc_info.frames]
                        .copy_from_slice(&in_channel_data[0..proc_info.frames]);

                    out_channel.set_constant(in_channel.is_constant());
                }
            }
        }
    }
}

impl Drop for PluginHostProcessor {
    fn drop(&mut self) {
        if self.thread_ids.is_process_thread() {
            if let ProcessingState::Started(_) = self.processing_state {
                self.plugin.stop_processing();
            }
        } else {
            log::error!("Plugin processor was not dropped in the process thread");
        }

        self.channel.shared_state.set_active_state(PluginActiveState::DroppedAndReadyToDeactivate);
    }
}
