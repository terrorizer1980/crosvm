// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the EDID specification provided by software.
//! EDID spec: <https://glenwing.github.io/docs/VESA-EEDID-A2.pdf>

use std::fmt;
use std::fmt::Debug;

use super::protocol::GpuResponse::*;
use super::protocol::VirtioGpuResult;

const EDID_DATA_LENGTH: usize = 128;
const DEFAULT_HORIZONTAL_BLANKING: u16 = 560;
const DEFAULT_VERTICAL_BLANKING: u16 = 50;
const DEFAULT_HORIZONTAL_FRONT_PORCH: u16 = 64;
const DEFAULT_VERTICAL_FRONT_PORCH: u16 = 1;
const DEFAULT_HORIZONTAL_SYNC_PULSE: u16 = 192;
const DEFAULT_VERTICAL_SYNC_PULSE: u16 = 3;

/// This class is used to create the Extended Display Identification Data (EDID), which will be
/// exposed to the guest system.
///
/// We ignore most of the spec, the point here being for us to provide enough for graphics to work
/// and to allow us to configure the resolution and refresh rate (via the preferred timing mode
/// pixel clock).
///
/// The EDID spec defines a number of methods to provide mode information, but in priority order the
/// "detailed" timing information is first, so we provide a single block of detailed timing
/// information and no other form of timing information.
#[repr(C)]
pub struct EdidBytes {
    bytes: [u8; EDID_DATA_LENGTH],
}

impl EdidBytes {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Debug for EdidBytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.bytes[..].fmt(f)
    }
}

impl PartialEq for EdidBytes {
    fn eq(&self, other: &EdidBytes) -> bool {
        self.bytes[..] == other.bytes[..]
    }
}

#[derive(Copy, Clone)]
pub struct Resolution {
    width: u32,
    height: u32,
}

impl Resolution {
    fn new(width: u32, height: u32) -> Resolution {
        Resolution { width, height }
    }

    fn get_aspect_ratio(&self) -> (u32, u32) {
        let divisor = gcd(self.width, self.height);
        (self.width / divisor, self.height / divisor)
    }
}

fn gcd(x: u32, y: u32) -> u32 {
    match y {
        0 => x,
        _ => gcd(y, x % y),
    }
}

#[derive(Copy, Clone)]
pub struct DisplayInfo {
    resolution: Resolution,
    refresh_rate: u32,
    horizontal_blanking: u16,
    vertical_blanking: u16,
    horizontal_front: u16,
    vertical_front: u16,
    horizontal_sync: u16,
    vertical_sync: u16,
}

impl DisplayInfo {
    /// Only width, height and refresh rate are required for the graphics stack to work, so instead
    /// of pulling actual numbers from the system, we just use some typical values to populate other
    /// fields for now.
    pub fn new(width: u32, height: u32, refresh_rate: u32) -> Self {
        Self {
            resolution: Resolution::new(width, height),
            refresh_rate,
            horizontal_blanking: DEFAULT_HORIZONTAL_BLANKING,
            vertical_blanking: DEFAULT_VERTICAL_BLANKING,
            horizontal_front: DEFAULT_HORIZONTAL_FRONT_PORCH,
            vertical_front: DEFAULT_VERTICAL_FRONT_PORCH,
            horizontal_sync: DEFAULT_HORIZONTAL_SYNC_PULSE,
            vertical_sync: DEFAULT_VERTICAL_SYNC_PULSE,
        }
    }

    pub fn width(&self) -> u32 {
        self.resolution.width
    }

    pub fn height(&self) -> u32 {
        self.resolution.height
    }
}

impl EdidBytes {
    /// Creates a virtual EDID block.
    pub fn new(info: &DisplayInfo) -> VirtioGpuResult {
        let mut edid: [u8; EDID_DATA_LENGTH] = [0; EDID_DATA_LENGTH];

        populate_header(&mut edid);
        populate_edid_version(&mut edid);
        populate_standard_timings(&mut edid)?;

        // 4 available descriptor blocks
        let block0 = &mut edid[54..72];
        populate_detailed_timing(block0, info);

        let block1 = &mut edid[72..90];
        populate_display_name(block1);

        calculate_checksum(&mut edid);

        Ok(OkEdid(Self { bytes: edid }))
    }
}

fn populate_display_name(edid_block: &mut [u8]) {
    // Display Product Name String Descriptor Tag
    edid_block[0..5].clone_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
    edid_block[5..].clone_from_slice("CrosvmDisplay".as_bytes());
}

fn populate_detailed_timing(edid_block: &mut [u8], info: &DisplayInfo) {
    assert_eq!(edid_block.len(), 18);

    // Detailed timings
    //
    // 18 Byte Descriptors - 72 Bytes
    // The 72 bytes in this section are divided into four data fields. Each of the four data fields
    // are 18 bytes in length. These 18 byte data fields shall contain either detailed timing data
    // as described in Section 3.10.2 or other types of data as described in Section 3.10.3. The
    // addresses and the contents of the four 18 byte descriptors are shown in Table 3.20.
    //
    // We leave the bottom 6 bytes of this block purposefully empty.
    let horizontal_blanking_lsb: u8 = (info.horizontal_blanking & 0xFF) as u8;
    let horizontal_blanking_msb: u8 = ((info.horizontal_blanking >> 8) & 0x0F) as u8;

    let vertical_blanking_lsb: u8 = (info.vertical_blanking & 0xFF) as u8;
    let vertical_blanking_msb: u8 = ((info.vertical_blanking >> 8) & 0x0F) as u8;

    // The pixel clock is what controls the refresh timing information.
    //
    // The formula for getting refresh rate out of this value is:
    //   refresh_rate = clk * 10000 / (htotal * vtotal)
    // Solving for clk:
    //   clk = (refresh_rate * htotal * votal) / 10000
    //
    // where:
    //   clk - The setting here
    //   vtotal - Total lines
    //   htotal - Total pixels per line
    //
    // Value here is pixel clock + 10,000, in 10khz steps.
    //
    // Pseudocode of kernel logic for vrefresh:
    //    vtotal := mode->vtotal;
    //    calc_val := (clock * 1000) / htotal
    //    refresh := (calc_val + vtotal / 2) / vtotal
    //    if flags & INTERLACE: refresh *= 2
    //    if flags & DBLSCAN: refresh /= 2
    //    if vscan > 1: refresh /= vscan
    //
    let htotal = info.width() + (info.horizontal_blanking as u32);
    let vtotal = info.height() + (info.vertical_blanking as u32);
    let mut clock: u16 = ((info.refresh_rate * htotal * vtotal) / 10000) as u16;
    // Round to nearest 10khz.
    clock = ((clock + 5) / 10) * 10;
    edid_block[0..2].copy_from_slice(&clock.to_le_bytes());

    let width_lsb: u8 = (info.width() & 0xFF) as u8;
    let width_msb: u8 = ((info.width() >> 8) & 0x0F) as u8;

    // Horizointal Addressable Video in pixels.
    edid_block[2] = width_lsb;
    // Horizontal blanking in pixels.
    edid_block[3] = horizontal_blanking_lsb;
    // Upper bits of the two above vals.
    edid_block[4] = horizontal_blanking_msb | (width_msb << 4) as u8;

    let vertical_active: u32 = info.height();
    let vertical_active_lsb: u8 = (vertical_active & 0xFF) as u8;
    let vertical_active_msb: u8 = ((vertical_active >> 8) & 0x0F) as u8;

    // Vertical addressable video in *lines*
    edid_block[5] = vertical_active_lsb;
    // Vertical blanking in lines
    edid_block[6] = vertical_blanking_lsb;
    // Sigbits of the above.
    edid_block[7] = vertical_blanking_msb | (vertical_active_msb << 4);

    let horizontal_front_lsb: u8 = (info.horizontal_front & 0xFF) as u8; // least sig 8 bits
    let horizontal_front_msb: u8 = ((info.horizontal_front >> 8) & 0x03) as u8; // most sig 2 bits
    let horizontal_sync_lsb: u8 = (info.horizontal_sync & 0xFF) as u8; // least sig 8 bits
    let horizontal_sync_msb: u8 = ((info.horizontal_sync >> 8) & 0x03) as u8; // most sig 2 bits

    let vertical_front_lsb: u8 = (info.vertical_front & 0x0F) as u8; // least sig 4 bits
    let vertical_front_msb: u8 = ((info.vertical_front >> 8) & 0x0F) as u8; // most sig 2 bits
    let vertical_sync_lsb: u8 = (info.vertical_sync & 0xFF) as u8; // least sig 4 bits
    let vertical_sync_msb: u8 = ((info.vertical_sync >> 8) & 0x0F) as u8; // most sig 2 bits

    // Horizontal front porch in pixels.
    edid_block[8] = horizontal_front_lsb;
    // Horizontal sync pulse width in pixels.
    edid_block[9] = horizontal_sync_lsb;
    // LSB of vertical front porch and sync pulse
    edid_block[10] = vertical_sync_lsb | (vertical_front_lsb << 4);
    // Upper 2 bits of these values.
    edid_block[11] = vertical_sync_msb
        | (vertical_front_msb << 2)
        | (horizontal_sync_msb << 4)
        | (horizontal_front_msb << 6);
}

// The EDID header. This is defined by the EDID spec.
fn populate_header(edid: &mut [u8]) {
    edid[0] = 0x00;
    edid[1] = 0xFF;
    edid[2] = 0xFF;
    edid[3] = 0xFF;
    edid[4] = 0xFF;
    edid[5] = 0xFF;
    edid[6] = 0xFF;
    edid[7] = 0x00;

    let manufacturer_name: [char; 3] = ['G', 'G', 'L'];
    // 00001 -> A, 00010 -> B, etc
    let manufacturer_id: u16 = manufacturer_name
        .iter()
        .map(|c| (*c as u8 - b'A' + 1) & 0x1F)
        .fold(0u16, |res, lsb| (res << 5) | (lsb as u16));
    edid[8..10].copy_from_slice(&manufacturer_id.to_be_bytes());

    let manufacture_product_id: u16 = 1;
    edid[10..12].copy_from_slice(&manufacture_product_id.to_le_bytes());

    let serial_id: u32 = 1;
    edid[12..16].copy_from_slice(&serial_id.to_le_bytes());

    let manufacture_week: u8 = 8;
    edid[16] = manufacture_week;

    let manufacture_year: u32 = 2022;
    edid[17] = (manufacture_year - 1990u32) as u8;
}

// The standard timings are 8 timing modes with a lower priority (and different data format)
// than the 4 detailed timing modes.
fn populate_standard_timings(edid: &mut [u8]) -> VirtioGpuResult {
    let resolutions = [
        Resolution::new(1440, 900),
        Resolution::new(1600, 900),
        Resolution::new(800, 600),
        Resolution::new(1680, 1050),
        Resolution::new(1856, 1392),
        Resolution::new(1280, 1024),
        Resolution::new(1400, 1050),
        Resolution::new(1920, 1200),
    ];

    // Index 0 is horizontal pixels / 8 - 31
    // Index 1 is a combination of the refresh_rate - 60 (so we are setting to 0, for now) and two
    // bits for the aspect ratio.
    for (index, r) in resolutions.iter().enumerate() {
        edid[0x26 + (index * 2)] = (r.width / 8 - 31) as u8;
        let ar_bits = match r.get_aspect_ratio() {
            (8, 5) => 0x0,
            (4, 3) => 0x1,
            (5, 4) => 0x2,
            (16, 9) => 0x3,
            (x, y) => return Err(ErrEdid(format!("Unsupported aspect ratio: {} {}", x, y))),
        };
        edid[0x27 + (index * 2)] = ar_bits;
    }
    Ok(OkNoData)
}

// Per the EDID spec, needs to be 1 and 4.
fn populate_edid_version(edid: &mut [u8]) {
    edid[18] = 1;
    edid[19] = 4;
}

fn calculate_checksum(edid: &mut [u8]) {
    let mut checksum: u8 = 0;
    for byte in edid.iter().take(EDID_DATA_LENGTH - 1) {
        checksum = checksum.wrapping_add(*byte);
    }

    if checksum != 0 {
        checksum = 255 - checksum + 1;
    }

    edid[127] = checksum;
}
