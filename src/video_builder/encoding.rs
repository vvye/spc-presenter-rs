use std::iter::zip;
use std::mem;
use std::str::FromStr;
use std::time::Duration;
use ffmpeg_next::{Dictionary, format, frame, Packet};
use crate::video_builder::ffmpeg_hacks::{ffmpeg_context_bytes_written, ffmpeg_sample_format_from_string};
use super::vb_unwrap::VideoBuilderUnwrap;
use super::VideoBuilder;

fn fast_background_blit(fg: &mut frame::Video, bg: &frame::Video) {
    const RB_MASK: u32 = 0xFF00FF;
    const G_MASK: u32 = 0x00FF00;

    for (fg_arr, bg_arr) in zip(fg.plane_mut::<[u8; 4]>(0).iter_mut(), bg.plane::<[u8; 4]>(0).iter()) {
        let fg_color = u32::from_le_bytes(*fg_arr) & (RB_MASK | G_MASK);

        let pre_blit_bg_arr = [bg_arr[0], bg_arr[1], bg_arr[2], 255];
        let bg_color = u32::from_le_bytes(pre_blit_bg_arr) & (RB_MASK | G_MASK);

        let mut rb = bg_color & RB_MASK;
        let mut g = bg_color & G_MASK;
        let a = fg_arr[3] as u32;

        rb += (((fg_color & RB_MASK) - rb) * a) >> 8;
        g += (((fg_color & G_MASK) - g) * a) >> 8;

        let o_color = (a << 24) | (rb & RB_MASK) | (g & G_MASK);
        fg_arr.copy_from_slice(&o_color.to_le_bytes());
    }
}

impl VideoBuilder {
    fn push_video_data_no_bg(&mut self, video: &[u8]) -> Result<(), String> {
        let mut input_frame = frame::Video::new(self.v_swc_ctx.input().format, self.v_swc_ctx.input().width, self.v_swc_ctx.input().height);
        input_frame.data_mut(0).copy_from_slice(video);

        let mut resize_frame = frame::Video::new(self.v_swc_ctx.output().format, self.v_swc_ctx.output().width, self.v_swc_ctx.output().height);
        self.v_swc_ctx.run(&input_frame, &mut resize_frame).vb_unwrap()?;

        let mut output_frame = frame::Video::new(self.v_sws_ctx.output().format, self.v_sws_ctx.output().width, self.v_sws_ctx.output().height);
        self.v_sws_ctx.run(&resize_frame, &mut output_frame).vb_unwrap()?;

        self.v_frame_buf.push_back(output_frame);

        Ok(())
    }

    fn push_video_data_bg(&mut self, video: &[u8]) -> Result<(), String> {
        let mut input_frame = frame::Video::new(self.v_sws_ctx.input().format, self.v_sws_ctx.input().width, self.v_sws_ctx.input().height);
        input_frame.data_mut(0).copy_from_slice(video);

        let mut resize_frame = frame::Video::new(self.v_sws_ctx.output().format, self.v_sws_ctx.output().width, self.v_sws_ctx.output().height);
        self.v_sws_ctx.run(&input_frame, &mut resize_frame).vb_unwrap()?;

        let background_frame = self.background.as_mut().unwrap().next_frame();
        fast_background_blit(&mut resize_frame, &background_frame);

        let mut output_frame = frame::Video::new(self.v_swc_ctx.output().format, self.v_swc_ctx.output().width, self.v_swc_ctx.output().height);
        self.v_swc_ctx.run(&resize_frame, &mut output_frame).vb_unwrap()?;

        self.v_frame_buf.push_back(output_frame);

        Ok(())
    }

    pub fn push_video_data(&mut self, video: &[u8]) -> Result<(), String> {
        if self.options.background_path.is_some() {
            self.push_video_data_bg(video)
        } else {
            self.push_video_data_no_bg(video)
        }
    }

    pub fn push_audio_data(&mut self, audio: &[u8]) -> Result<(), String> {
        let bytes_per_sample = self.a_swr_ctx.input().channel_layout.channels() as usize * self.a_swr_ctx.input().format.bytes();
        let samples = audio.len() / bytes_per_sample;

        let mut input_frame = frame::Audio::new(self.a_swr_ctx.input().format, samples, self.a_swr_ctx.input().channel_layout);
        input_frame.set_rate(self.options.sample_rate as _);
        input_frame.data_mut(0).copy_from_slice(audio);

        let mut output_frame = frame::Audio::new(self.a_swr_ctx.output().format, samples, self.a_swr_ctx.output().channel_layout);
        output_frame.set_rate(self.options.sample_rate as _);
        self.a_swr_ctx.run(&input_frame, &mut output_frame).vb_unwrap()?;

        self.a_frame_buf.push_back(output_frame);

        Ok(())
    }

    fn send_video_to_encoder(&mut self) -> Result<(), String> {
        if let Some(mut frame) = self.v_frame_buf.pop_front() {
            frame.set_pts(Some(self.v_pts));
            self.v_encoder.send_frame(&frame).vb_unwrap()?;

            self.v_pts += 1;
        }

        Ok(())
    }

    fn mux_video_frame(&mut self, packet: &mut Packet) -> Result<bool, String> {
        if self.v_encoder.receive_packet(packet).is_ok() {
            let out_time_base = self.out_ctx.stream(self.v_stream_idx)
                .unwrap()
                .time_base();

            packet.rescale_ts(self.options.video_time_base, out_time_base);
            packet.set_stream(self.v_stream_idx);
            packet.write_interleaved(&mut self.out_ctx).vb_unwrap()?;

            self.v_pts_muxed += 1;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn send_audio_to_encoder(&mut self) -> Result<(), String> {
        if let Some(mut frame) = self.a_frame_buf.pop_front() {
            frame.set_pts(Some(self.a_pts));
            self.a_encoder.send_frame(&frame).vb_unwrap()?;

            self.a_pts += self.a_encoder.frame_size() as i64;
        }

        Ok(())
    }

    fn mux_audio_frame(&mut self, packet: &mut Packet) -> Result<bool, String> {
        if self.a_encoder.receive_packet(packet).is_ok() {
            let out_time_base = self.out_ctx.stream(self.a_stream_idx)
                .unwrap()
                .time_base();

            packet.rescale_ts(self.options.audio_time_base, out_time_base);
            packet.set_stream(self.a_stream_idx);
            packet.write_interleaved(&mut self.out_ctx).vb_unwrap()?;

            self.a_pts_muxed += 1;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn start_encoding(&mut self) -> Result<(), String> {
        let mut opts = Dictionary::new();
        println!("{}", self.out_ctx.format().name());
        match self.out_ctx.format().name() {
            "mp4" => opts.set("movflags", "faststart"),
            _ => ()
        };

        self.out_ctx.write_header_with(opts).vb_unwrap()?;

        Ok(())
    }

    pub fn step_encoding(&mut self) -> Result<(), String> {
        let mut packet = Packet::empty();

        loop {
            if self.a_pts_muxed <= self.v_pts_muxed && !self.a_frame_buf.is_empty() {
                self.send_audio_to_encoder()?;
                if !(self.mux_audio_frame(&mut packet)?) {
                    break;
                }
            } else if !self.v_frame_buf.is_empty() {
                self.send_video_to_encoder()?;
                if !(self.mux_video_frame(&mut packet)?) {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(())
    }

    pub fn finish_encoding(&mut self) -> Result<(), String> {
        self.v_encoder.send_eof().vb_unwrap()?;
        self.a_encoder.send_eof().vb_unwrap()?;

        let mut packet = Packet::empty();
        loop {
            let muxed_audio = self.mux_audio_frame(&mut packet)?;
            let muxed_video = self.mux_video_frame(&mut packet)?;

            if !muxed_audio && !muxed_video {
                break;
            }
        }

        self.out_ctx.write_trailer().vb_unwrap()?;

        Ok(())
    }

    pub fn audio_frame_size(&self) -> usize {
        self.a_frame_size
    }

    pub fn encoded_video_duration(&self) -> Duration {
        let time_base_fraction = self.options.video_time_base.numerator() as f64 / self.options.video_time_base.denominator() as f64;
        let seconds = time_base_fraction * self.v_pts as f64;
        Duration::from_secs_f64(seconds)
    }

    pub fn encoded_video_size(&self) -> usize {
        ffmpeg_context_bytes_written(&self.out_ctx)
    }
}