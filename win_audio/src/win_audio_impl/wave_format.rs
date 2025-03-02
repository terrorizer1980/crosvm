// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::convert::TryInto;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;

use base::warn;
use metrics::event_details_proto::WaveFormat;
use metrics::event_details_proto::WaveFormatDetails;
use metrics::event_details_proto::WaveFormat_WaveFormatSubFormat;
use winapi::shared::guiddef::IsEqualGUID;
use winapi::shared::guiddef::GUID;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_ADPCM;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_ALAW;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_ANALOG;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_DRM;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_MPEG;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_MULAW;
use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_PCM;
use winapi::shared::mmreg::SPEAKER_FRONT_CENTER;
use winapi::shared::mmreg::SPEAKER_FRONT_LEFT;
use winapi::shared::mmreg::SPEAKER_FRONT_RIGHT;
use winapi::shared::mmreg::WAVEFORMATEX;
use winapi::shared::mmreg::WAVEFORMATEXTENSIBLE;
use winapi::shared::mmreg::WAVE_FORMAT_EXTENSIBLE;
use winapi::shared::mmreg::WAVE_FORMAT_IEEE_FLOAT;
#[cfg(not(test))]
use winapi::um::combaseapi::CoTaskMemFree;

use crate::AudioSharedFormat;
use crate::MONO_CHANNEL_COUNT;
use crate::STEREO_CHANNEL_COUNT;

pub type WaveFormatDetailsProto = WaveFormatDetails;
pub type WaveFormatProto = WaveFormat;
pub type SubFormatProto = WaveFormat_WaveFormatSubFormat;

/// Wrapper around `WAVEFORMATEX` and `WAVEFORMATEXTENSIBLE` to hide some of the unsafe calls
/// that could be made.
pub enum WaveAudioFormat {
    /// Format where channels are capped at 2.
    WaveFormat(WAVEFORMATEX),
    /// Format where channels can be >2. (It can still have <=2 channels)
    WaveFormatExtensible(WAVEFORMATEXTENSIBLE),
}

impl WaveAudioFormat {
    /// Wraps a WAVEFORMATEX pointer to make it's use more safe.
    ///
    /// # Safety
    /// Unsafe if `wave_format_ptr` is pointing to null. This function will assume it's not null
    /// and dereference it.
    /// Also `format_ptr` will be deallocated after this function completes, so it cannot be used.
    pub unsafe fn new(format_ptr: *mut WAVEFORMATEX) -> Self {
        let format_tag = { (*format_ptr).wFormatTag };
        let result = if format_tag != WAVE_FORMAT_EXTENSIBLE {
            warn!(
                "Default Mix Format does not have format_tag WAVE_FORMAT_EXTENSIBLE. It is: {}",
                format_tag
            );
            WaveAudioFormat::WaveFormat(*format_ptr)
        } else {
            WaveAudioFormat::WaveFormatExtensible(*(format_ptr as *const WAVEFORMATEXTENSIBLE))
        };

        // WAVEFORMATEX and WAVEFORMATEXTENSIBLE both implement the Copy trait, so they have been
        // copied to the WaveAudioFormat enum. Therefore, it is safe to free the memory
        // `format_ptr` is pointing to.
        // In a test, WAVEFORMATEX is initiated by us, not by Windows, so calling this function
        // could cause a STATUS_HEAP_CORRUPTION exception.
        #[cfg(not(test))]
        CoTaskMemFree(format_ptr as *mut std::ffi::c_void);

        result
    }

    pub fn get_num_channels(&self) -> u16 {
        match self {
            WaveAudioFormat::WaveFormat(wave_format) => wave_format.nChannels,
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                wave_format_extensible.Format.nChannels
            }
        }
    }

    // Modifies `WAVEFORMATEXTENSIBLE` to have the values passed into the function params.
    // Currently it should only modify the bit_depth if it's != 32 and the data format if it's not
    // float.
    pub fn modify_mix_format(&mut self, target_bit_depth: usize, ks_data_format: GUID) {
        let default_num_channels = self.get_num_channels();

        fn calc_avg_bytes_per_sec(num_channels: u16, bit_depth: u16, samples_per_sec: u32) -> u32 {
            num_channels as u32 * (bit_depth as u32 / 8) * samples_per_sec
        }

        fn calc_block_align(num_channels: u16, bit_depth: u16) -> u16 {
            (bit_depth / 8) * num_channels
        }

        match self {
            WaveAudioFormat::WaveFormat(wave_format) => {
                if default_num_channels > STEREO_CHANNEL_COUNT {
                    warn!("WAVEFORMATEX shouldn't have >2 channels.");
                }

                // Force the format to be the only supported format (32 bit float)
                if wave_format.wBitsPerSample != target_bit_depth as u16
                    || wave_format.wFormatTag != WAVE_FORMAT_IEEE_FLOAT
                {
                    wave_format.wFormatTag = WAVE_FORMAT_IEEE_FLOAT;
                    wave_format.nChannels =
                        std::cmp::min(STEREO_CHANNEL_COUNT as u16, default_num_channels);
                    wave_format.wBitsPerSample = target_bit_depth as u16;
                    wave_format.nAvgBytesPerSec = calc_avg_bytes_per_sec(
                        wave_format.nChannels,
                        wave_format.wBitsPerSample,
                        wave_format.nSamplesPerSec,
                    );
                    wave_format.nBlockAlign =
                        calc_block_align(wave_format.nChannels, wave_format.wBitsPerSample);
                }
            }
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                // WAVE_FORMAT_EXTENSIBLE uses the #[repr(packed)] flag so the compiler might
                // unalign the fields. Thus, the fields will be copied to a local variable to
                // prevent segfaults. For more information:
                // https://github.com/rust-lang/rust/issues/46043
                let sub_format = wave_format_extensible.SubFormat;

                if wave_format_extensible.Format.wBitsPerSample != target_bit_depth as u16
                    || !IsEqualGUID(&sub_format, &ks_data_format)
                {
                    // wFormatTag won't be changed
                    wave_format_extensible.Format.nChannels = default_num_channels;
                    wave_format_extensible.Format.wBitsPerSample = target_bit_depth as u16;
                    // nSamplesPerSec should stay the same
                    // Calculated with a bit depth of 32bits
                    wave_format_extensible.Format.nAvgBytesPerSec = calc_avg_bytes_per_sec(
                        wave_format_extensible.Format.nChannels,
                        wave_format_extensible.Format.wBitsPerSample,
                        wave_format_extensible.Format.nSamplesPerSec,
                    );
                    wave_format_extensible.Format.nBlockAlign = calc_block_align(
                        wave_format_extensible.Format.nChannels,
                        wave_format_extensible.Format.wBitsPerSample,
                    );
                    // 22 is the size typically used when the format tag is WAVE_FORMAT_EXTENSIBLE.
                    // Since the `Initialize` syscall takes in a WAVEFORMATEX, this tells Windows
                    // how many bytes are left after the `Format` field
                    // (ie. Samples, dwChannelMask, SubFormat) so that it can cast to
                    // WAVEFORMATEXTENSIBLE safely.
                    wave_format_extensible.Format.cbSize = 22;
                    wave_format_extensible.Samples = target_bit_depth as u16;
                    let n_channels = wave_format_extensible.Format.nChannels;
                    // The channel masks are defined here:
                    // https://docs.microsoft.com/en-us/windows/win32/api/mmreg/ns-mmreg-waveformatextensible#remarks
                    wave_format_extensible.dwChannelMask = match n_channels {
                        STEREO_CHANNEL_COUNT => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
                        MONO_CHANNEL_COUNT => SPEAKER_FRONT_CENTER,
                        _ => {
                            // Don't change channel mask if it's >2 channels.
                            wave_format_extensible.dwChannelMask
                        }
                    };
                    wave_format_extensible.SubFormat = ks_data_format;
                }
            }
        }
    }

    pub fn as_ptr(&self) -> *const WAVEFORMATEX {
        match self {
            WaveAudioFormat::WaveFormat(wave_format) => wave_format as *const WAVEFORMATEX,
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                wave_format_extensible as *const _ as *const WAVEFORMATEX
            }
        }
    }

    pub fn get_shared_audio_engine_period_in_frames(
        &self,
        shared_default_size_in_100nanoseconds: f64,
    ) -> usize {
        // a 100 nanosecond unit is 1 * 10^-7 seconds
        //
        // To convert a 100nanoseconds value to # of frames in a period, we multiple by the
        // frame rate (nSamplesPerSec. Sample rate == Frame rate) and then divide by 10000000
        // in order to convert 100nanoseconds to seconds.
        let samples_per_sec = match self {
            WaveAudioFormat::WaveFormat(wave_format) => wave_format.nSamplesPerSec,
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                wave_format_extensible.Format.nSamplesPerSec
            }
        };

        ((samples_per_sec as f64 * shared_default_size_in_100nanoseconds as f64) / 10000000.0)
            .ceil() as usize
    }

    pub fn create_audio_shared_format(
        &self,
        shared_audio_engine_period_in_frames: usize,
    ) -> AudioSharedFormat {
        match self {
            WaveAudioFormat::WaveFormat(wave_format) => AudioSharedFormat {
                bit_depth: wave_format.wBitsPerSample as usize,
                frame_rate: wave_format.nSamplesPerSec as usize,
                shared_audio_engine_period_in_frames,
                channels: wave_format.nChannels as usize,
                channel_mask: None,
            },
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => AudioSharedFormat {
                bit_depth: wave_format_extensible.Format.wBitsPerSample as usize,
                frame_rate: wave_format_extensible.Format.nSamplesPerSec as usize,
                shared_audio_engine_period_in_frames,
                channels: wave_format_extensible.Format.nChannels as usize,
                channel_mask: Some(wave_format_extensible.dwChannelMask),
            },
        }
    }

    #[cfg(test)]
    fn take_waveformatex(self) -> WAVEFORMATEX {
        match self {
            WaveAudioFormat::WaveFormat(wave_format) => wave_format,
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                // Safe because `wave_format_extensible` can't be a null pointer, otherwise the
                // constructor would've failed. This will also give the caller ownership of this
                // struct.
                unsafe { *(&wave_format_extensible as *const _ as *const WAVEFORMATEX) }
            }
        }
    }

    #[cfg(test)]
    fn take_waveformatextensible(self) -> WAVEFORMATEXTENSIBLE {
        match self {
            WaveAudioFormat::WaveFormat(_wave_format) => {
                panic!("Format is WAVEFORMATEX. Can't convert to WAVEFORMATEXTENSBILE.")
            }
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => wave_format_extensible,
        }
    }
}

impl Debug for WaveAudioFormat {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let res = match self {
            WaveAudioFormat::WaveFormat(wave_format) => {
                format!(
                    "wFormatTag: {}, \nnChannels: {}, \nnSamplesPerSec: {}, \nnAvgBytesPerSec: \
                    {}, \nnBlockAlign: {}, \nwBitsPerSample: {}, \ncbSize: {}",
                    { wave_format.wFormatTag },
                    { wave_format.nChannels },
                    { wave_format.nSamplesPerSec },
                    { wave_format.nAvgBytesPerSec },
                    { wave_format.nBlockAlign },
                    { wave_format.wBitsPerSample },
                    { wave_format.cbSize },
                )
            }
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                let audio_engine_format = format!(
                    "wFormatTag: {}, \nnChannels: {}, \nnSamplesPerSec: {}, \nnAvgBytesPerSec: \
                    {}, \nnBlockAlign: {}, \nwBitsPerSample: {}, \ncbSize: {}",
                    { wave_format_extensible.Format.wFormatTag },
                    { wave_format_extensible.Format.nChannels },
                    { wave_format_extensible.Format.nSamplesPerSec },
                    { wave_format_extensible.Format.nAvgBytesPerSec },
                    { wave_format_extensible.Format.nBlockAlign },
                    { wave_format_extensible.Format.wBitsPerSample },
                    { wave_format_extensible.Format.cbSize },
                );

                let subformat = wave_format_extensible.SubFormat;

                if !IsEqualGUID(
                    &{ wave_format_extensible.SubFormat },
                    &KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
                ) {
                    warn!("Audio Engine format is NOT IEEE FLOAT");
                }

                let audio_engine_extensible_format = format!(
                    "\nSamples: {}, \ndwChannelMask: {}, \nSubFormat: {}-{}-{}-{:?}",
                    { wave_format_extensible.Samples },
                    { wave_format_extensible.dwChannelMask },
                    subformat.Data1,
                    subformat.Data2,
                    subformat.Data3,
                    subformat.Data4,
                );

                format!("{}{}", audio_engine_format, audio_engine_extensible_format)
            }
        };
        write!(f, "{}", res)
    }
}

#[cfg(test)]
impl PartialEq for WaveAudioFormat {
    fn eq(&self, other: &Self) -> bool {
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            return false;
        }

        fn are_formats_same(
            wave_format_pointer: *const u8,
            other_format_pointer: *const u8,
            cb_size: usize,
        ) -> bool {
            let wave_format_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    wave_format_pointer,
                    std::mem::size_of::<WAVEFORMATEX>() + cb_size,
                )
            };
            let other_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    other_format_pointer,
                    std::mem::size_of::<WAVEFORMATEX>() + cb_size,
                )
            };

            !wave_format_bytes
                .iter()
                .zip(other_bytes)
                .map(|(x, y)| x.cmp(y))
                .any(|ord| ord != std::cmp::Ordering::Equal)
        }

        match self {
            WaveAudioFormat::WaveFormat(wave_format) => match other {
                WaveAudioFormat::WaveFormat(other_wave_format) => {
                    if wave_format.cbSize != other_wave_format.cbSize {
                        return false;
                    }
                    are_formats_same(
                        wave_format as *const _ as *const u8,
                        other_wave_format as *const _ as *const u8,
                        wave_format.cbSize as usize,
                    )
                }
                WaveAudioFormat::WaveFormatExtensible(_) => unreachable!(),
            },
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => match other {
                WaveAudioFormat::WaveFormatExtensible(other_wave_format_extensible) => {
                    if wave_format_extensible.Format.cbSize
                        != other_wave_format_extensible.Format.cbSize
                    {
                        return false;
                    }
                    are_formats_same(
                        wave_format_extensible as *const _ as *const u8,
                        other_wave_format_extensible as *const _ as *const u8,
                        wave_format_extensible.Format.cbSize as usize,
                    )
                }
                WaveAudioFormat::WaveFormat(_) => unreachable!(),
            },
        }
    }
}

impl From<&WaveAudioFormat> for WaveFormatProto {
    fn from(format: &WaveAudioFormat) -> WaveFormatProto {
        let mut wave_format_proto = WaveFormatProto::new();

        match format {
            WaveAudioFormat::WaveFormat(wave_format) => {
                wave_format_proto.set_format_tag(wave_format.wFormatTag.into());
                wave_format_proto.set_channels(wave_format.nChannels.into());
                wave_format_proto.set_samples_per_sec(
                    wave_format
                        .nSamplesPerSec
                        .try_into()
                        .expect("Failed to cast nSamplesPerSec to i32"),
                );
                wave_format_proto.set_avg_bytes_per_sec(
                    wave_format
                        .nAvgBytesPerSec
                        .try_into()
                        .expect("Failed to cast nAvgBytesPerSec"),
                );
                wave_format_proto.set_block_align(wave_format.nBlockAlign.into());
                wave_format_proto.set_bits_per_sample(wave_format.wBitsPerSample.into());
                wave_format_proto.set_size_bytes(wave_format.cbSize.into());
            }
            WaveAudioFormat::WaveFormatExtensible(wave_format_extensible) => {
                wave_format_proto.set_format_tag(wave_format_extensible.Format.wFormatTag.into());
                wave_format_proto.set_channels(wave_format_extensible.Format.nChannels.into());
                wave_format_proto.set_samples_per_sec(
                    wave_format_extensible
                        .Format
                        .nSamplesPerSec
                        .try_into()
                        .expect("Failed to cast nSamplesPerSec to i32"),
                );
                wave_format_proto.set_avg_bytes_per_sec(
                    wave_format_extensible
                        .Format
                        .nAvgBytesPerSec
                        .try_into()
                        .expect("Failed to cast nAvgBytesPerSec"),
                );
                wave_format_proto.set_block_align(wave_format_extensible.Format.nBlockAlign.into());
                wave_format_proto
                    .set_bits_per_sample(wave_format_extensible.Format.wBitsPerSample.into());
                wave_format_proto.set_size_bytes(wave_format_extensible.Format.cbSize.into());
                wave_format_proto.set_samples(wave_format_extensible.Samples.into());
                wave_format_proto.set_channel_mask(wave_format_extensible.dwChannelMask.into());
                let sub_format = wave_format_extensible.SubFormat;
                wave_format_proto.set_sub_format(GuidWrapper(&sub_format).into());
            }
        }

        wave_format_proto
    }
}

struct GuidWrapper<'a>(&'a GUID);

impl<'a> From<GuidWrapper<'a>> for SubFormatProto {
    fn from(guid: GuidWrapper) -> SubFormatProto {
        let guid = guid.0;
        if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_ANALOG) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_ANALOG
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_PCM) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_PCM
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_DRM) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_DRM
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_ALAW) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_ALAW
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_MULAW) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_MULAW
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_ADPCM) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_ADPCM
        } else if IsEqualGUID(guid, &KSDATAFORMAT_SUBTYPE_MPEG) {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_MPEG
        } else {
            SubFormatProto::KSDATAFORMAT_SUBTYPE_INVALID
        }
    }
}

#[cfg(test)]
mod tests {
    use winapi::shared::ksmedia::KSDATAFORMAT_SUBTYPE_PCM;
    use winapi::shared::mmreg::SPEAKER_BACK_LEFT;
    use winapi::shared::mmreg::SPEAKER_BACK_RIGHT;
    use winapi::shared::mmreg::SPEAKER_LOW_FREQUENCY;
    use winapi::shared::mmreg::SPEAKER_SIDE_LEFT;
    use winapi::shared::mmreg::SPEAKER_SIDE_RIGHT;
    use winapi::shared::mmreg::WAVE_FORMAT_PCM;

    use super::*;

    #[test]
    fn test_modify_mix_format() {
        // A typical 7.1 surround sound channel mask.
        const channel_mask_7_1: u32 = SPEAKER_FRONT_LEFT
            | SPEAKER_FRONT_RIGHT
            | SPEAKER_FRONT_CENTER
            | SPEAKER_LOW_FREQUENCY
            | SPEAKER_BACK_LEFT
            | SPEAKER_BACK_RIGHT
            | SPEAKER_SIDE_LEFT
            | SPEAKER_SIDE_RIGHT;

        let surround_sound_format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 8,
                nSamplesPerSec: 44100,
                nAvgBytesPerSec: 1411200,
                nBlockAlign: 32,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: 32,
            dwChannelMask: channel_mask_7_1,
            SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
        };

        // Safe because `GetMixFormat` casts `WAVEFORMATEXTENSIBLE` into a `WAVEFORMATEX` like so.
        // Also this is casting from a bigger to a smaller struct, so it shouldn't be possible for
        // this contructor to access memory it shouldn't.
        let mut format = unsafe {
            WaveAudioFormat::new((&surround_sound_format) as *const _ as *mut WAVEFORMATEX)
        };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        // Safe because we know the format is originally a `WAVEFORMATEXTENSIBLE`.
        let surround_sound_format = format.take_waveformatextensible();

        // WAVE_FORMAT_EXTENSIBLE uses the #[repr(packed)] flag so the compiler might unalign
        // the fields. Thus, the fields will be copied to a local variable to prevent segfaults.
        // For more information: https://github.com/rust-lang/rust/issues/46043
        let format_tag = surround_sound_format.Format.wFormatTag;
        // We expect `SubFormat` to be IEEE float instead of PCM.
        // Everything else should remain the same.
        assert_eq!(format_tag, WAVE_FORMAT_EXTENSIBLE);
        let channels = surround_sound_format.Format.nChannels;
        assert_eq!(channels, 8);
        let samples_per_sec = surround_sound_format.Format.nSamplesPerSec;
        assert_eq!(samples_per_sec, 44100);
        let avg_bytes_per_sec = surround_sound_format.Format.nAvgBytesPerSec;
        assert_eq!(avg_bytes_per_sec, 1411200);
        let block_align = surround_sound_format.Format.nBlockAlign;
        assert_eq!(block_align, 32);
        let bits_per_samples = surround_sound_format.Format.wBitsPerSample;
        assert_eq!(bits_per_samples, 32);
        let size = surround_sound_format.Format.cbSize;
        assert_eq!(size, 22);
        let samples = surround_sound_format.Samples;
        assert_eq!(samples, 32);
        let channel_mask = surround_sound_format.dwChannelMask;
        assert_eq!(channel_mask, channel_mask_7_1);
        let sub_format = surround_sound_format.SubFormat;
        assert!(IsEqualGUID(&sub_format, &KSDATAFORMAT_SUBTYPE_IEEE_FLOAT));
    }

    #[test]
    fn test_waveformatex_ieee_modify_same_format() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 384000,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let mut format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        let result_format = format.take_waveformatex();

        assert_waveformatex_ieee(&result_format);
    }

    #[test]
    fn test_waveformatex_ieee_modify_different_format() {
        // I don't expect this format to show up ever, but it's possible so it's good to test.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 192000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let mut format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        let result_format = format.take_waveformatex();

        assert_waveformatex_ieee(&result_format);
    }

    #[test]
    fn test_format_comparison_waveformatex_pass() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 1,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 4 * 48000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        let expected = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 1,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 4 * 48000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        let expected = unsafe {
            WaveAudioFormat::new((&expected) as *const WAVEFORMATEX as *mut WAVEFORMATEX)
        };

        assert_eq!(expected, format);
    }

    #[test]
    fn test_format_comparison_waveformatextensible_pass() {
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 1,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 4 * 48000,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 22,
            },
            Samples: 16,
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let format = unsafe {
            WaveAudioFormat::new((&format) as *const WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX)
        };

        let expected = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 1,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 4 * 48000,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 22,
            },
            Samples: 16,
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        let expected = unsafe {
            WaveAudioFormat::new((&expected) as *const WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX)
        };

        assert_eq!(expected, format);
    }

    #[test]
    fn test_format_comparison_waveformatex_fail() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 1,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 4 * 48000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        let expected = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            // The field below is the difference
            nChannels: 6,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 4 * 48000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        let expected = unsafe {
            WaveAudioFormat::new((&expected) as *const WAVEFORMATEX as *mut WAVEFORMATEX)
        };

        assert_ne!(expected, format);
    }

    #[test]
    fn test_format_comparison_waveformatextensible_fail() {
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 1,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 4 * 48000,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 22,
            },
            Samples: 16,
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let format = unsafe {
            WaveAudioFormat::new((&format) as *const WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX)
        };

        let expected = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 1,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 4 * 48000,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 22,
            },
            Samples: 16,
            // The field below is the difference.
            dwChannelMask: SPEAKER_FRONT_CENTER | SPEAKER_BACK_LEFT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        let expected = unsafe {
            WaveAudioFormat::new((&expected) as *const WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX)
        };

        assert_ne!(expected, format);
    }

    #[test]
    fn test_modify_mix_mono_channel_different_bit_depth_wave_format_extensible() {
        // Start with a mono channel and 16 bit depth format.
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 1,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 2 * 48000,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 22,
            },
            Samples: 16,
            // Probably will never see a mask like this for two channels, but this is just testing
            // that it will get changed.
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let mut format = unsafe {
            WaveAudioFormat::new((&format) as *const WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX)
        };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        // The format should be converted to 32 bit depth and retain mono channel.
        let expected = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 1,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 4 * 48000, // Changed
                nBlockAlign: 4,             // Changed
                wBitsPerSample: 32,         // Changed
                cbSize: 22,
            },
            Samples: 32,
            dwChannelMask: SPEAKER_FRONT_CENTER, // Changed
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        let expected = unsafe {
            WaveAudioFormat::new((&expected) as *const WAVEFORMATEXTENSIBLE as *mut WAVEFORMATEX)
        };

        assert_eq!(format, expected);
    }

    #[test]
    fn test_modify_mix_mono_channel_different_bit_depth_wave_format() {
        // Start with a mono channel and 16 bit depth format.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 1,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 2 * 48000,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let mut format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        // The format should be converted to 32 bit depth and retain mono channel.
        let expected = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT, // Changed
            nChannels: 1,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 4 * 48000, // Changed
            nBlockAlign: 4,             // Changed
            wBitsPerSample: 32,         // Changed
            cbSize: 0,
        };

        let expected = unsafe {
            WaveAudioFormat::new((&expected) as *const WAVEFORMATEX as *mut WAVEFORMATEX)
        };

        assert_eq!(format, expected);
    }

    #[test]
    fn test_waveformatex_non_ieee_modify_format() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 192000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let mut format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        let result_format = format.take_waveformatex();

        assert_waveformatex_ieee(&result_format);
    }

    #[test]
    fn test_waveformatex_non_ieee_32_bit_modify_format() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 384000,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let mut format =
            unsafe { WaveAudioFormat::new((&format) as *const WAVEFORMATEX as *mut WAVEFORMATEX) };

        format.modify_mix_format(
            /* bit_depth= */ 32,
            /* ks_data_format= */ KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        );

        let result_format = format.take_waveformatex();

        assert_waveformatex_ieee(&result_format);
    }

    fn assert_waveformatex_ieee(result_format: &WAVEFORMATEX) {
        let format_tag = result_format.wFormatTag;
        assert_eq!(format_tag, WAVE_FORMAT_IEEE_FLOAT);
        let channels = result_format.nChannels;
        assert_eq!(channels, 2);
        let samples_per_sec = result_format.nSamplesPerSec;
        assert_eq!(samples_per_sec, 48000);
        let avg_bytes_per_sec = result_format.nAvgBytesPerSec;
        assert_eq!(avg_bytes_per_sec, 384000);
        let block_align = result_format.nBlockAlign;
        assert_eq!(block_align, 8);
        let bits_per_samples = result_format.wBitsPerSample;
        assert_eq!(bits_per_samples, 32);
        let size = result_format.cbSize;
        assert_eq!(size, 0);
    }

    #[test]
    fn test_create_audio_shared_format_wave_format_ex() {
        let wave_format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 192000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let format = unsafe {
            WaveAudioFormat::new((&wave_format) as *const WAVEFORMATEX as *mut WAVEFORMATEX)
        };

        // The period will most likely never be 123, but this is ok for testing.
        let audio_shared_format =
            format.create_audio_shared_format(/* shared_audio_engine_period_in_frames= */ 123);

        assert_eq!(
            audio_shared_format.bit_depth,
            wave_format.wBitsPerSample as usize
        );
        assert_eq!(audio_shared_format.channels, wave_format.nChannels as usize);
        assert_eq!(
            audio_shared_format.frame_rate,
            wave_format.nSamplesPerSec as usize
        );
        assert_eq!(
            audio_shared_format.shared_audio_engine_period_in_frames,
            123
        );
    }

    #[test]
    fn test_create_audio_shared_format_wave_format_extensible() {
        let wave_format_extensible = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 2,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 8 * 48000,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: 32,
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let format = unsafe {
            WaveAudioFormat::new((&wave_format_extensible) as *const _ as *mut WAVEFORMATEX)
        };

        // The period will most likely never be 123, but this is ok for testing.
        let audio_shared_format =
            format.create_audio_shared_format(/* shared_audio_engine_period_in_frames= */ 123);

        assert_eq!(
            audio_shared_format.bit_depth,
            wave_format_extensible.Format.wBitsPerSample as usize
        );
        assert_eq!(
            audio_shared_format.channels,
            wave_format_extensible.Format.nChannels as usize
        );
        assert_eq!(
            audio_shared_format.frame_rate,
            wave_format_extensible.Format.nSamplesPerSec as usize
        );
        assert_eq!(
            audio_shared_format.shared_audio_engine_period_in_frames,
            123
        );
        assert_eq!(
            audio_shared_format.channel_mask,
            Some(SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT)
        );
    }

    #[test]
    fn test_wave_format_to_proto_convertion() {
        let wave_format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 192000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let wave_audio_format =
            unsafe { WaveAudioFormat::new((&wave_format) as *const _ as *mut WAVEFORMATEX) };

        // Testing the `into`.
        let wave_format_proto = WaveFormatProto::from(&wave_audio_format);

        let mut expected = WaveFormatProto::new();
        expected.set_format_tag(WAVE_FORMAT_PCM.into());
        expected.set_channels(2);
        expected.set_samples_per_sec(48000);
        expected.set_avg_bytes_per_sec(192000);
        expected.set_block_align(4);
        expected.set_bits_per_sample(16);
        expected.set_size_bytes(0);

        assert_eq!(wave_format_proto, expected);
    }

    #[test]
    fn test_wave_format_extensible_to_proto_convertion() {
        let wave_format_extensible = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 2,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 8 * 48000,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: 32,
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        // Safe because we can convert a struct to a pointer declared above. Also that means the
        // pointer can be safely deferenced.
        let wave_audio_format = unsafe {
            WaveAudioFormat::new((&wave_format_extensible) as *const _ as *mut WAVEFORMATEX)
        };

        // Testing the `into`.
        let wave_format_proto = WaveFormatProto::from(&wave_audio_format);

        let mut expected = WaveFormatProto::new();
        expected.set_format_tag(WAVE_FORMAT_EXTENSIBLE.into());
        expected.set_channels(2);
        expected.set_samples_per_sec(48000);
        expected.set_avg_bytes_per_sec(8 * 48000);
        expected.set_block_align(8);
        expected.set_bits_per_sample(32);
        expected.set_size_bytes(22);
        expected.set_samples(32);
        expected.set_channel_mask((SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT) as i64);
        expected.set_sub_format(GuidWrapper(&KSDATAFORMAT_SUBTYPE_IEEE_FLOAT).into());

        assert_eq!(wave_format_proto, expected);
    }
}
