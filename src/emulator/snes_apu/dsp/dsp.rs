use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use pitch_detection::detector::PitchDetector;
use pitch_detection::detector::yin::YINDetector;
use crate::emulator::ApuStateReceiver;
use crate::emulator::snes_apu::dsp::brr_block_decoder::BrrBlockDecoder;
use super::super::apu::Apu;
use super::voice::{Voice, ResamplingMode};
use super::filter::Filter;
use super::ring_buffer::RingBuffer;
use super::super::spc::spc::{Spc, REG_LEN};
use super::dsp_helpers;

pub const SAMPLE_RATE: usize = 32000;
pub const BUFFER_LEN: usize = SAMPLE_RATE * 2;

const NUM_VOICES: usize = 8;

const COUNTER_RANGE: i32 = 30720;
static COUNTER_RATES: [i32; 32] = [
    COUNTER_RANGE + 1, // Never fires
    2048, 1536, 1280, 1024, 768, 640, 512, 384, 320, 256, 192, 160, 128, 96,
    80, 64, 48, 40, 32, 24, 20, 16, 12, 10, 8, 6, 5, 4, 3, 2, 1];

static COUNTER_OFFSETS: [i32; 32] = [
    1, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040,
    536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 0, 0];

pub struct Dsp {
    emulator: *mut Apu,

    pub voices: Vec<Box<Voice>>,

    left_filter: Filter,
    right_filter: Filter,
    pub output_buffer: RingBuffer,

    vol_left: u8,
    vol_right: u8,
    echo_vol_left: u8,
    echo_vol_right: u8,
    noise_clock: u8,
    echo_write_enabled: bool,
    echo_feedback: u8,
    source_dir: u8,
    echo_start_address: u16,
    echo_delay: u8,
    kon_cache: u8,
    koff_cache: u8,

    counter: i32,

    cycles_since_last_flush: i32,
    is_flushing: bool,
    noise: i32,
    echo_pos: i32,
    echo_length: i32,

    resampling_mode: ResamplingMode,

    pub state_receiver: Option<Rc<RefCell<dyn ApuStateReceiver>>>,
    pub source_pitches: HashMap<u8, f64>
}

impl Dsp {
    pub fn new(emulator: *mut Apu) -> Box<Dsp> {
        let resampling_mode = ResamplingMode::Gaussian;
        let mut ret = Box::new(Dsp {
            emulator: emulator,

            voices: Vec::with_capacity(NUM_VOICES),

            left_filter: Filter::new(),
            right_filter: Filter::new(),
            output_buffer: RingBuffer::new(),

            vol_left: 0x89,
            vol_right: 0x9c,
            echo_vol_left: 0x9f,
            echo_vol_right: 0x9c,
            noise_clock: 0,
            echo_write_enabled: false,
            echo_feedback: 0,
            source_dir: 0,
            echo_start_address: Dsp::calculate_echo_start_address(0x60),
            echo_delay: 0x0e,
            kon_cache: 0,
            koff_cache: 0,

            counter: 0,

            cycles_since_last_flush: 0,
            is_flushing: false,
            noise: 0x4000,
            echo_pos: 0,
            echo_length: 0,

            resampling_mode: resampling_mode,

            state_receiver: None,
            source_pitches: HashMap::new()
        });
        let ret_ptr = &mut *ret as *mut _;
        for _ in 0..NUM_VOICES {
            ret.voices.push(Box::new(Voice::new(ret_ptr, emulator, resampling_mode)));
        }
        ret.set_filter_coefficient(0x00, 0x80);
        ret.set_filter_coefficient(0x01, 0xff);
        ret.set_filter_coefficient(0x02, 0x9a);
        ret.set_filter_coefficient(0x03, 0xff);
        ret.set_filter_coefficient(0x04, 0x67);
        ret.set_filter_coefficient(0x05, 0xff);
        ret.set_filter_coefficient(0x06, 0x0f);
        ret.set_filter_coefficient(0x07, 0xff);
        ret.set_resampling_mode(ResamplingMode::Gaussian);
        ret
    }

    #[inline]
    fn emulator(&self) -> &mut Apu {
        unsafe {
            &mut (*self.emulator)
        }
    }

    fn set_filter_coefficient(&mut self, index: i32, value: u8) {
        self.left_filter.coefficients[index as usize] = value;
        self.right_filter.coefficients[index as usize] = value;
    }

    fn get_filter_coefficient(&self, index: i32) -> u8 {
        self.left_filter.coefficients[index as usize]
    }

    pub fn resampling_mode(&self) -> ResamplingMode {
        self.resampling_mode
    }

    pub fn set_resampling_mode(&mut self, resampling_mode: ResamplingMode) {
        self.resampling_mode = resampling_mode;
        for voice in self.voices.iter_mut() {
            voice.resampling_mode = resampling_mode;
        }
    }

    fn calculate_echo_start_address(value: u8) -> u16 {
        (value as u16) << 8
    }

    pub fn set_state(&mut self, spc: &Spc) {
        for i in 0..REG_LEN {
            match i {
                0x4c | 0x5c => (), // Do nothing
                _ => { self.set_register(i as u8, spc.regs[i as usize]); }
            }
        }

        self.set_kon(spc.regs[0x4c]);
    }

    pub fn cycles_callback(&mut self, num_cycles: i32) {
        self.cycles_since_last_flush += num_cycles;
    }

    pub fn get_echo_start_address(&self) -> u16 {
        self.echo_start_address
    }

    pub fn calculate_echo_length(&self) -> i32 {
        (self.echo_delay as i32) * 0x800
    }

    pub fn flush(&mut self) {
        self.is_flushing = true;

        while self.cycles_since_last_flush > 64 {
            if !self.read_counter(self.noise_clock as i32) {
                let feedback = (self.noise << 13) ^ (self.noise << 14);
                self.noise = (feedback & 0x4000) ^ (self.noise >> 1);
            }

            let mut are_any_voices_solod = false;
            for voice in self.voices.iter() {
                if voice.is_solod {
                    are_any_voices_solod = true;
                    break;
                }
            }

            let mut left_out = 0;
            let mut right_out = 0;
            let mut left_echo_out = 0;
            let mut right_echo_out = 0;
            let mut last_voice_out = 0;
            for voice in self.voices.iter_mut() {
                let output = voice.render_sample(last_voice_out, self.noise, are_any_voices_solod);

                left_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(left_out + output.left_out, 17));
                right_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(right_out + output.right_out, 17));

                if voice.echo_on {
                    left_echo_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(left_echo_out + output.left_out, 17));
                    right_echo_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(right_echo_out + output.right_out, 17));
                }

                last_voice_out = ((output.last_voice_out & 0xFFFF) as i16) as i32;
            }

            left_out = dsp_helpers::multiply_volume(left_out, self.vol_left);
            right_out = dsp_helpers::multiply_volume(right_out, self.vol_right);

            let echo_address = (self.echo_start_address.wrapping_add(self.echo_pos as u16)) as u32;
            // println!("ECHO_ADDR=${:04x} ECHO_LEN=${:04x} ESA=${:04x} EDL=${:02x}\n\n\n", echo_address, self.echo_length, self.echo_start_address, self.echo_delay);
            let mut left_echo_in = (((((self.emulator().read_u8(echo_address + 1) as i32) << 8) | (self.emulator().read_u8(echo_address) as i32)) as i16) & !1) as i32;
            let mut right_echo_in = (((((self.emulator().read_u8(echo_address + 3) as i32) << 8) | (self.emulator().read_u8(echo_address + 2) as i32)) as i16) & !1) as i32;

            left_echo_in = dsp_helpers::clamp(self.left_filter.next(left_echo_in, false)) & !1;
            right_echo_in = dsp_helpers::clamp(self.right_filter.next(right_echo_in, true)) & !1;

            let left_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(left_out + dsp_helpers::multiply_volume(left_echo_in, self.echo_vol_left), 17)) as i16;
            let right_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(right_out + dsp_helpers::multiply_volume(right_echo_in, self.echo_vol_right), 17)) as i16;
            self.output_buffer.write_sample(left_out, right_out);

            if self.echo_write_enabled {
                left_echo_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(left_echo_out + ((((left_echo_in * ((self.echo_feedback as i8) as i32)) >> 7) as i16) as i32), 17)) & !1;
                right_echo_out = dsp_helpers::clamp(dsp_helpers::cast_arb_int(right_echo_out + ((((right_echo_in * ((self.echo_feedback as i8) as i32)) >> 7) as i16) as i32), 17)) & !1;

                self.emulator().write_u8(echo_address + 0, left_echo_out as u8);
                self.emulator().write_u8(echo_address + 1, (left_echo_out >> 8) as u8);
                self.emulator().write_u8(echo_address + 2, right_echo_out as u8);
                self.emulator().write_u8(echo_address + 3, (right_echo_out >> 8) as u8);
            }
            if self.echo_pos == 0 {
                self.echo_length = self.calculate_echo_length();
            }
            self.echo_pos += 4;
            if self.echo_pos >= self.echo_length {
                self.echo_pos = 0;
            }

            self.counter = (self.counter + 1) % COUNTER_RANGE;
            self.cycles_since_last_flush -= 64;

            if self.state_receiver.is_some() {
                for channel in 0..NUM_VOICES {
                    // Need to do this first to avoid double mutable borrow
                    let source_pitch = self.detect_voice_pitch(channel);

                    let voice = self.voices.get_mut(channel).unwrap();

                    let volume = {
                        if voice.is_muted {
                            0u8
                        } else {
                            // (2.25 * (((voice.vol_left / 2) + (voice.vol_right / 2)) as f64 + 1.0).log2() * (voice.envelope.level as f64 / 2047.0)).ceil() as u8
                            // (4.0 * (((voice.vol_left as f64 / 2.0) + (voice.vol_right as f64 / 2.0)).abs() / 8.0 + 1.0).log2() * (voice.envelope.level as f64 / 2047.0)).round() as u8
                            (2.8 * (((voice.vol_left as f64 / 2.0) + (voice.vol_right as f64 / 2.0)).abs() / 3.0 + 1.0).log2() * (voice.envelope.level as f64 / 2047.0)).ceil() as u8
                        }
                    };
                    let timbre = voice.source as usize;
                    let balance = ((voice.vol_left as f64).abs() / -128.0) + ((voice.vol_right as f64).abs() / 128.0) + 0.5;
                    let edge = voice.edge_detected();

                    let l_last_sample = voice.output_buffer.read().left_out;
                    let r_last_sample = voice.output_buffer.read().right_out;
                    let amplitude = {
                        if (voice.vol_left as i8) < 0 && (voice.vol_right as i8) > 0 {
                            ((r_last_sample - l_last_sample) / 2) as i16
                        } else if (voice.vol_left as i8) > 0 && (voice.vol_right as i8) < 0 {
                            ((l_last_sample - r_last_sample) / 2) as i16
                        } else {
                            ((l_last_sample + r_last_sample) / 2) as i16
                        }
                    };

                    let frequency = match voice.noise_on {
                        true => source_pitch,
                        false => source_pitch * (voice.pitch() as f64) / (0x1000 as f64)
                    };
                    let kon_frames = voice.get_sample_frame();

                    self.state_receiver.clone().unwrap().borrow_mut().receive(channel, volume, amplitude, frequency, timbre, balance, edge, kon_frames);
                }
            }
        }

        self.is_flushing = false;
    }

    pub fn set_register(&mut self, address: u8, value: u8) {
        if (address & 0x80) != 0 {
            return;
        }

        if !self.is_flushing {
            self.flush();
        }

        let voice_index = address >> 4;
        let voice_address = address & 0x0f;
        if voice_address < 0x0a {
            if voice_address < 8 {
                let voice = &mut self.voices[voice_index as usize];
                match voice_address {
                    0x00 => { voice.vol_left = value; },
                    0x01 => { voice.vol_right = value; },
                    0x02 => { voice.pitch_low = value; },
                    0x03 => { voice.set_pitch_high(value); },
                    0x04 => { voice.source = value; voice.edge_hit = true; },
                    0x05 => { voice.envelope.adsr0 = value; },
                    0x06 => { voice.envelope.adsr1 = value; },
                    0x07 => { voice.envelope.gain = value; },
                    _ => () // Do nothing
                }
            }
        } else if voice_address == 0x0f {
            self.set_filter_coefficient(voice_index as i32, value);
        } else {
            match address {
                0x0c => { self.vol_left = value; },
                0x1c => { self.vol_right = value; },
                0x2c => { self.echo_vol_left = value; },
                0x3c => { self.echo_vol_right = value; },
                0x4c => { self.set_kon(value); },
                0x5c => { self.set_kof(value); },
                0x6c => { self.set_flg(value); },
                0x7c => { self.set_endx(); },

                0x0d => { self.echo_feedback = value; },

                0x2d => { self.set_pmon(value); },
                0x3d => { self.set_nov(value); },
                0x4d => { self.set_eon(value); },
                0x5d => { self.source_dir = value; },
                0x6d => { self.echo_start_address = (value as u16) << 8; },
                0x7d => { self.echo_delay = value & 0x0f; self.echo_length = self.calculate_echo_length(); },

                _ => () // Do nothing
            }
        }
    }

    pub fn get_register(&mut self, address: u8) -> u8 {
        if !self.is_flushing {
            self.flush();
        }

        let voice_index = address >> 4;
        let voice_address = address & 0x0f;
        if voice_address < 0x0a {
            let voice = &mut self.voices[voice_index as usize];
            match voice_address {
                0x00 => voice.vol_left,
                0x01 => voice.vol_right,
                0x02 => voice.pitch_low,
                0x03 => voice.pitch_high & 0x3F,
                0x04 => voice.source,
                0x05 => voice.envelope.adsr0,
                0x06 => voice.envelope.adsr1,
                0x07 => voice.envelope.gain,
                0x08 => (voice.envelope.level >> 4) as u8,
                0x09 => voice.outx_value,
                _ => unreachable!()
            }
        } else if voice_address == 0x0f {
            self.get_filter_coefficient(voice_index as i32)
        } else {
            match address {
                0x0c => self.vol_left,
                0x1c => self.vol_right,
                0x2c => self.echo_vol_left,
                0x3c => self.echo_vol_right,
                0x4c => self.kon_cache,
                0x5c => self.koff_cache,
                0x6c => self.get_flg(),
                0x7c => self.get_endx(),

                0x2d => self.get_pmon(),
                0x3d => self.get_nov(),
                0x4d => self.get_eon(),
                0x5d => self.source_dir,
                0x6d => (self.echo_start_address >> 8) as u8,
                0x7d => self.echo_delay,

                _ => 0
            }
        }
    }

    pub fn read_counter(&self, rate: i32) -> bool {
        ((self.counter + COUNTER_OFFSETS[rate as usize]) % COUNTER_RATES[rate as usize]) != 0
    }

    pub fn read_source_dir_start_address(&self, index: i32) -> u32 {
        self.read_source_dir_address(index, 0)
    }

    pub fn read_source_dir_loop_address(&self, index: i32) -> u32 {
        self.read_source_dir_address(index, 2)
    }

    fn read_source_dir_address(&self, index: i32, offset: i32) -> u32 {
        let dir_address = (self.source_dir as i32) * 0x100;
        let entry_address = dir_address + index * 4;
        let mut ret = self.emulator().read_u8((entry_address as u32) + (offset as u32)) as u32;
        ret |= (self.emulator().read_u8((entry_address as u32) + (offset as u32) + 1) as u32) << 8;
        ret
    }

    fn set_kon(&mut self, voice_mask: u8) {
        self.kon_cache = voice_mask;
        for i in 0..NUM_VOICES {
            if ((voice_mask as usize) & (1 << i)) != 0 {
                self.voices[i].key_on();
            }
        }
    }

    fn set_kof(&mut self, voice_mask: u8) {
        self.koff_cache = voice_mask;
        for i in 0..NUM_VOICES {
            if ((voice_mask as usize) & (1 << i)) != 0 {
                self.voices[i].key_off();
            }
        }
    }

    fn set_flg(&mut self, value: u8) {
        self.noise_clock = value & 0x1f;
        self.echo_write_enabled = (value & 0x20) == 0;
    }

    fn get_flg(&self) -> u8 {
        let mut result = self.noise_clock;
        if self.echo_write_enabled {
            result |= 0x20;
        }
        result
    }

    fn set_pmon(&mut self, voice_mask: u8) {
        for i in 1..NUM_VOICES {
            self.voices[i].pitch_mod = ((voice_mask as usize) & (1 << i)) != 0;
        }
    }

    fn get_pmon(&self) -> u8 {
        let mut result = 0u8;
        for i in 1..NUM_VOICES {
            if self.voices[i].pitch_mod {
                result |= (1 << i) as u8;
            }
        }
        result
    }

    fn set_nov(&mut self, voice_mask: u8) {
        for i in 0..NUM_VOICES {
            self.voices[i].noise_on = ((voice_mask as usize) & (1 << i)) != 0;
        }
    }

    fn get_nov(&self) -> u8 {
        let mut result = 0u8;
        for i in 1..NUM_VOICES {
            if self.voices[i].noise_on {
                result |= (1 << i) as u8;
            }
        }
        result
    }

    fn set_eon(&mut self, voice_mask: u8) {
        for i in 0..NUM_VOICES {
            self.voices[i].echo_on = ((voice_mask as usize) & (1 << i)) != 0;
        }
    }

    fn get_eon(&self) -> u8 {
        let mut result = 0u8;
        for i in 1..NUM_VOICES {
            if self.voices[i].echo_on {
                result |= (1 << i) as u8;
            }
        }
        result
    }

    fn set_endx(&mut self) {
        for i in 0..NUM_VOICES {
            self.voices[i].clear_endx_bit();
        }
    }

    fn get_endx(&mut self) -> u8 {
        let mut result = 0u8;
        for i in 0..NUM_VOICES {
            if self.voices[i].get_endx_bit() {
                result |= (1 << (i as u8));
            }
        }
        result
    }

    fn detect_voice_pitch(&mut self, channel: usize) -> f64 {
        if self.voices[channel].noise_on {
            const C_0: f64 = 16.351597831287;

            return C_0 * (2.0_f64).powf((self.noise_clock as f64) / 12.0);
        }

        let source = self.voices[channel].source;

        if let Some(pitch) = self.source_pitches.get(&source) {
            return *pitch;
        }

        let mut decoded_sample: Vec<f64> = Vec::new();
        let mut sample_address = self.read_source_dir_start_address(source as i32);
        let loop_address = self.read_source_dir_loop_address(source as i32);

        let mut brr_block_decoder = BrrBlockDecoder::new();
        let mut loop_count = 0;
        let mut start_block_count = 0;
        let mut loop_block_count = 0;

        brr_block_decoder.reset(0, 0);

        loop {
            let mut buf = [0; 9];
            for i in 0..9 {
                buf[i] = self.emulator().read_u8(sample_address + i as u32);
            }
            brr_block_decoder.read(&buf);
            sample_address += 9;

            match loop_count {
                0 => start_block_count += 1,
                1 => loop_block_count += 1,
                _ => ()
            };

            while !brr_block_decoder.is_finished() {
                decoded_sample.push(brr_block_decoder.read_next_sample() as f64);
            }

            if brr_block_decoder.is_end {
                // Loop for 5 seconds
                if brr_block_decoder.is_looping && decoded_sample.len() < (10 * 32000) {
                    sample_address = loop_address;
                    loop_count += 1;
                } else {
                    break;
                }
            }
        }

        let mut detector = YINDetector::new(decoded_sample.len(), decoded_sample.len() / 2);
        let (pitch, confidence) = match detector.get_pitch(&decoded_sample, 32000, 6.0, 0.5) {
            Some(pitch) => {
                let mut frequency = pitch.frequency;
                // Samples probably aren't going to have a fundamental period longer than 16 BRR blocks (256 sample points, 125 Hz),
                // so we can use this way-too-simple heuristic to detect pitch predictions that are too low.
                // This is entirely arbitrary - it just so happens to make a lot of visualizations look better.
                while frequency < 125.0 {
                    frequency *= 2.0;
                }
                (frequency, pitch.clarity)
            },
            None => {
                let mut period_blocks = match loop_block_count {
                    0 => start_block_count,
                    _ => loop_block_count
                }.max(1);

                while period_blocks > 16 {
                    period_blocks /= 2;
                }

                println!("WARNING: YIN failure! Assuming period is {} BRR blocks", period_blocks);

                (32000.0 / (period_blocks * 16) as f64, 0.0)
            }
        };

        self.source_pitches.insert(source, pitch);
        println!("Detected new source ${:x}, f0={} Hz, clarity={}, length={}:{}\n\n\n", source, pitch, confidence, start_block_count, loop_block_count);
        pitch
    }
}
