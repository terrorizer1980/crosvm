// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap as Map;
use std::fmt;
use std::fmt::Display;
#[cfg(windows)]
use std::marker::PhantomData;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use serde_keyvalue::FromKeyValues;

pub use crate::sys::DisplayMode;

pub use crate::sys::handle_request;
pub use crate::*;

pub const DEFAULT_DISPLAY_WIDTH: u32 = 1280;
pub const DEFAULT_DISPLAY_HEIGHT: u32 = 1024;
pub const DEFAULT_REFRESH_RATE: u32 = 60;

fn default_refresh_rate() -> u32 {
    DEFAULT_REFRESH_RATE
}

/// Trait that the platform-specific type `DisplayMode` needs to implement.
pub trait DisplayModeTrait {
    fn get_virtual_display_size(&self) -> (u32, u32);
}

impl Default for DisplayMode {
    fn default() -> Self {
        Self::Windowed(DEFAULT_DISPLAY_WIDTH, DEFAULT_DISPLAY_HEIGHT)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, FromKeyValues, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct DisplayParameters {
    #[serde(default)]
    pub mode: DisplayMode,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default = "default_refresh_rate")]
    pub refresh_rate: u32,
}

impl DisplayParameters {
    pub fn new(mode: DisplayMode, hidden: bool, refresh_rate: u32) -> Self {
        Self {
            mode,
            hidden,
            refresh_rate,
        }
    }

    pub fn default_with_mode(mode: DisplayMode) -> Self {
        Self::new(mode, false, DEFAULT_REFRESH_RATE)
    }

    pub fn get_virtual_display_size(&self) -> (u32, u32) {
        self.mode.get_virtual_display_size()
    }
}

impl Default for DisplayParameters {
    fn default() -> Self {
        Self::default_with_mode(Default::default())
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum GpuControlCommand {
    AddDisplays { displays: Vec<DisplayParameters> },
    ListDisplays,
    RemoveDisplays { display_ids: Vec<u32> },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum GpuControlResult {
    DisplaysUpdated,
    DisplayList {
        displays: Map<u32, DisplayParameters>,
    },
    TooManyDisplays(usize),
    NoSuchDisplay {
        display_id: u32,
    },
}

impl Display for GpuControlResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::GpuControlResult::*;

        match self {
            DisplaysUpdated => write!(f, "displays updated"),
            DisplayList { displays } => {
                let json: serde_json::Value = serde_json::json!({
                    "displays": displays,
                });
                let json_pretty =
                    serde_json::to_string_pretty(&json).map_err(|_| std::fmt::Error)?;
                write!(f, "{}", json_pretty)
            }
            TooManyDisplays(n) => write!(f, "too_many_displays {}", n),
            NoSuchDisplay { display_id } => write!(f, "no_such_display {}", display_id),
        }
    }
}

pub enum ModifyGpuError {
    SocketFailed,
    UnexpectedResponse(VmResponse),
    UnknownCommand(String),
    GpuControl(GpuControlResult),
}

impl fmt::Display for ModifyGpuError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ModifyGpuError::*;

        match self {
            SocketFailed => write!(f, "socket failed"),
            UnexpectedResponse(r) => write!(f, "unexpected response: {}", r),
            UnknownCommand(c) => write!(f, "unknown display command: `{}`", c),
            GpuControl(e) => write!(f, "{}", e),
        }
    }
}

pub type ModifyGpuResult = std::result::Result<GpuControlResult, ModifyGpuError>;

impl From<VmResponse> for ModifyGpuResult {
    fn from(response: VmResponse) -> Self {
        match response {
            VmResponse::GpuResponse(gpu_response) => Ok(gpu_response),
            r => Err(ModifyGpuError::UnexpectedResponse(r)),
        }
    }
}

pub fn do_gpu_display_add<T: AsRef<Path> + std::fmt::Debug>(
    control_socket_path: T,
    displays: Vec<DisplayParameters>,
) -> ModifyGpuResult {
    let request = VmRequest::GpuCommand(GpuControlCommand::AddDisplays { displays });
    handle_request(&request, control_socket_path)
        .map_err(|_| ModifyGpuError::SocketFailed)?
        .into()
}

pub fn do_gpu_display_list<T: AsRef<Path> + std::fmt::Debug>(
    control_socket_path: T,
) -> ModifyGpuResult {
    let request = VmRequest::GpuCommand(GpuControlCommand::ListDisplays);
    handle_request(&request, control_socket_path)
        .map_err(|_| ModifyGpuError::SocketFailed)?
        .into()
}

pub fn do_gpu_display_remove<T: AsRef<Path> + std::fmt::Debug>(
    control_socket_path: T,
    display_ids: Vec<u32>,
) -> ModifyGpuResult {
    let request = VmRequest::GpuCommand(GpuControlCommand::RemoveDisplays { display_ids });
    handle_request(&request, control_socket_path)
        .map_err(|_| ModifyGpuError::SocketFailed)?
        .into()
}
