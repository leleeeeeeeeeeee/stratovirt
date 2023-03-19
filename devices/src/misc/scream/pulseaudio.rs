// Copyright (c) 2023 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::sync::atomic::{fence, Ordering};

use log::{error, warn};
use psimple::Simple;
use pulse::{
    channelmap::{Map, MapDef, Position},
    def::BufferAttr,
    sample::{Format, Spec},
    stream::Direction,
    time::MicroSeconds,
};

use crate::misc::scream::{ScreamDirection, ShmemStreamFmt, StreamData};

const AUDIO_SAMPLE_RATE_44KHZ: u32 = 44100;
const AUDIO_SAMPLE_RATE_48KHZ: u32 = 48000;
const WINDOWS_SAMPLE_BASE_RATE: u8 = 128;

pub const TAGET_LATENCY_MS: u32 = 50;
const MAX_LATENCY_MS: u32 = 100;

const STREAM_NAME: &str = "Audio";

const WINDOWS_POSITION_CNT: usize = 11;
const PULSEAUDIO_POSITION: [Position; WINDOWS_POSITION_CNT] = [
    Position::FrontLeft,
    Position::FrontRight,
    Position::FrontCenter,
    Position::Lfe,
    Position::RearLeft,
    Position::RearRight,
    Position::FrontLeftOfCenter,
    Position::FrontRightOfCenter,
    Position::RearCenter,
    Position::SideLeft,
    Position::SideRight,
];

impl ScreamDirection {
    fn transform(&self) -> Direction {
        match self {
            Self::Playback => Direction::Playback,
            Self::Record => Direction::Record,
        }
    }
}

/// Data structure of the audio processed by the pulseaudio.
pub struct PulseStreamData {
    simple: Option<Simple>,
    ss: Spec,
    channel_map: Map,
    buffer_attr: BufferAttr,
    stream_fmt: ShmemStreamFmt,
    latency: u32,
    app_name: String,
    stream_name: String,
    dir: Direction,
}

impl PulseStreamData {
    pub fn init(name: &str, dir: ScreamDirection) -> Self {
        // Map to stereo, it's the default number of channels.
        let mut channel_map = Map::default();
        channel_map.init_stereo();

        // Start with base default format, rate and channels. Will switch to actual format later.
        let ss = Spec {
            format: Format::S16le,
            rate: AUDIO_SAMPLE_RATE_44KHZ,
            channels: 2,
        };

        // Init receiver format to track changes.
        let stream_fmt = ShmemStreamFmt::default();

        // Set buffer size for requested latency.
        let buffer_attr = BufferAttr {
            maxlength: ss.usec_to_bytes(MicroSeconds(MAX_LATENCY_MS as u64 * 1000)) as u32,
            tlength: ss.usec_to_bytes(MicroSeconds(TAGET_LATENCY_MS as u64 * 1000)) as u32,
            prebuf: std::u32::MAX,
            minreq: std::u32::MAX,
            fragsize: std::u32::MAX,
        };

        let pa_dir = dir.transform();

        let simple = Simple::new(
            None,
            name,
            pa_dir,
            None,
            STREAM_NAME,
            &ss,
            Some(&channel_map),
            Some(&buffer_attr),
        )
        .unwrap_or_else(|e| panic!("PulseAudio init failed : {}", e));

        Self {
            simple: Some(simple),
            ss,
            channel_map,
            buffer_attr,
            stream_fmt,
            latency: TAGET_LATENCY_MS,
            app_name: name.to_string(),
            stream_name: STREAM_NAME.to_string(),
            dir: pa_dir,
        }
    }

    fn transfer_channel_map(&mut self, format: &ShmemStreamFmt) {
        self.channel_map.init();
        self.channel_map.set_len(format.channels);
        let map: &mut [Position] = self.channel_map.get_mut();
        /* In Windows, the channel mask shows as following figure.
         *   31    11   10   9   8     7    6   5    4     3   2     1   0
         *  |     |  | SR | SL | BC | FRC| FLC| BR | BL | LFE| FC | FR | FL |
         *
         *  Each bit in the channel mask represents a particular speaker position.
         *  Now, it map a windows SPEAKER_* position to a PA_CHANNEL_POSITION_*.
         */
        let mut key: i32 = -1;
        for (i, item) in map.iter_mut().enumerate().take(format.channels as usize) {
            for j in (key + 1)..32 {
                if (format.channel_map >> j) & 0x01 == 1 {
                    key = j;
                    break;
                }
            }
            // Map the key value to a pulseaudio channel position.
            if (key as usize) < WINDOWS_POSITION_CNT {
                *item = PULSEAUDIO_POSITION[key as usize];
            } else {
                warn!("Channel {} can not be mapped, Falling back to 'center'.", i);
                *item = Position::FrontCenter;
            }
        }
    }

    fn check_fmt_update(&mut self, recv_data: &StreamData) {
        if self.stream_fmt == recv_data.fmt {
            return;
        }
        // If audio format changed, reconfigure
        self.stream_fmt = recv_data.fmt;
        self.ss.channels = recv_data.fmt.channels;
        self.ss.rate = if recv_data.fmt.rate >= WINDOWS_SAMPLE_BASE_RATE {
            AUDIO_SAMPLE_RATE_44KHZ
        } else {
            AUDIO_SAMPLE_RATE_48KHZ
        } * (recv_data.fmt.rate % WINDOWS_SAMPLE_BASE_RATE) as u32;

        match recv_data.fmt.size {
            16 => self.ss.format = Format::S16le,
            24 => self.ss.format = Format::S24le,
            32 => self.ss.format = Format::S32le,
            _ => {
                warn!(
                    "Unsuported sample size {}, not playing until next format switch",
                    recv_data.fmt.size
                );
                self.ss.rate = 0;
            }
        }

        if recv_data.fmt.channels == 1 {
            self.channel_map.init_mono();
        } else if recv_data.fmt.channels == 2 {
            self.channel_map.init_stereo();
        } else {
            self.transfer_channel_map(&recv_data.fmt);
        }

        if !self.channel_map.is_valid() {
            warn!("Invalid channel mapping, falling back to MapDef::WAVEEx");
            self.channel_map
                .init_extend(recv_data.fmt.channels, MapDef::WAVEEx);
        }
        if !self.channel_map.is_compatible_with_sample_spec(&self.ss) {
            warn!("Incompatible channel mapping.");
            self.ss.rate = 0;
        }

        if self.ss.rate > 0 {
            // Sample spec has changed, so the playback buffer size for the requested latency must be recalculated as well.
            self.buffer_attr.tlength =
                self.ss
                    .usec_to_bytes(MicroSeconds(self.latency as u64 * 1000)) as u32;

            self.simple = Simple::new(
                None,
                self.app_name.as_str(),
                self.dir,
                None,
                self.stream_name.as_str(),
                &self.ss,
                Some(&self.channel_map),
                Some(&self.buffer_attr),
            )
            .map_or_else(
                |_| {
                    warn!(
                "Unable to open PulseAudio with sample rate {}, sample size {} and channels {}",
                self.ss.rate, recv_data.fmt.size, recv_data.fmt.channels
            );
                    None
                },
                Some,
            );
        }
    }

    pub fn send(&mut self, recv_data: &StreamData) {
        self.check_fmt_update(recv_data);

        if self.ss.rate == 0 || self.simple.is_none() {
            return;
        }

        // Make sure audio read does not bypass chunk_idx read.
        fence(Ordering::Acquire);

        // SAFETY: audio_base is the shared memory. It already verifies the validity
        // of the address range during the header check.
        let data = unsafe {
            std::slice::from_raw_parts(
                recv_data.audio_base as *const u8,
                recv_data.audio_size as usize,
            )
        };

        if let Err(e) = self.simple.as_ref().unwrap().write(data) {
            error!("PulseAudio write data failed: {}", e);
        }
    }

    pub fn receive(&mut self, recv_data: &StreamData) -> bool {
        self.check_fmt_update(recv_data);

        if self.simple.is_none() {
            return false;
        }

        // SAFETY: audio_base is the shared memory. It already verifies the validity
        // of the address range during the header check.
        let data = unsafe {
            std::slice::from_raw_parts_mut(
                recv_data.audio_base as *mut u8,
                recv_data.audio_size as usize,
            )
        };

        if let Err(e) = self.simple.as_ref().unwrap().read(data) {
            error!("PulseAudio read data failed: {}", e);
            self.ss.rate = 0;
            return false;
        }

        true
    }
}
