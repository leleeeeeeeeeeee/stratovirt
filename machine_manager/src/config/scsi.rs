// Copyright (c) 2022 Huawei Technologies Co.,Ltd. All rights reserved.
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

use anyhow::{anyhow, Result};

use super::{error::ConfigError, pci_args_check};
use crate::config::{CmdParser, ConfigCheck, VmConfig, MAX_STRING_LENGTH, MAX_VIRTIO_QUEUE};

/// According to Virtio Spec.
/// Max_channel should be 0.
/// Max_target should be less than or equal to 255.
pub const VIRTIO_SCSI_MAX_TARGET: u16 = 255;
/// Max_lun should be less than or equal to 16383 (2^14 - 1).
pub const VIRTIO_SCSI_MAX_LUN: u16 = 16383;

/// Only support peripheral device addressing format(8 bits for lun) in stratovirt now.
/// So, max lun id supported is 255 (2^8 - 1).
const SUPPORT_SCSI_MAX_LUN: u16 = 255;

#[derive(Debug, Clone)]
pub struct ScsiCntlrConfig {
    /// Virtio-scsi-pci device id.
    pub id: String,
    /// Thread name of io handler.
    pub iothread: Option<String>,
    /// Number of scsi cmd queues.
    pub queues: u32,
}

impl Default for ScsiCntlrConfig {
    fn default() -> Self {
        ScsiCntlrConfig {
            id: "".to_string(),
            iothread: None,
            //At least 1 cmd queue.
            queues: 1,
        }
    }
}

impl ConfigCheck for ScsiCntlrConfig {
    fn check(&self) -> Result<()> {
        if self.id.len() > MAX_STRING_LENGTH {
            return Err(anyhow!(ConfigError::StringLengthTooLong(
                "virtio-scsi-pci device id".to_string(),
                MAX_STRING_LENGTH,
            )));
        }

        if self.iothread.is_some() && self.iothread.as_ref().unwrap().len() > MAX_STRING_LENGTH {
            return Err(anyhow!(ConfigError::StringLengthTooLong(
                "iothread name".to_string(),
                MAX_STRING_LENGTH,
            )));
        }

        if self.queues < 1 || self.queues > MAX_VIRTIO_QUEUE as u32 {
            return Err(anyhow!(ConfigError::IllegalValue(
                "queues number of scsi controller".to_string(),
                1,
                true,
                MAX_VIRTIO_QUEUE as u64,
                true,
            )));
        }

        Ok(())
    }
}

pub fn parse_scsi_controller(
    drive_config: &str,
    queues_auto: Option<u16>,
) -> Result<ScsiCntlrConfig> {
    let mut cmd_parser = CmdParser::new("virtio-scsi-pci");
    cmd_parser
        .push("")
        .push("id")
        .push("bus")
        .push("addr")
        .push("multifunction")
        .push("iothread")
        .push("num-queues");

    cmd_parser.parse(drive_config)?;

    pci_args_check(&cmd_parser)?;

    let mut cntlr_cfg = ScsiCntlrConfig::default();

    if let Some(iothread) = cmd_parser.get_value::<String>("iothread")? {
        cntlr_cfg.iothread = Some(iothread);
    }

    if let Some(id) = cmd_parser.get_value::<String>("id")? {
        cntlr_cfg.id = id;
    } else {
        return Err(anyhow!(ConfigError::FieldIsMissing(
            "id",
            "virtio scsi pci"
        )));
    }

    if let Some(queues) = cmd_parser.get_value::<u32>("num-queues")? {
        cntlr_cfg.queues = queues;
    } else if let Some(queues) = queues_auto {
        cntlr_cfg.queues = queues as u32;
    }

    cntlr_cfg.check()?;
    Ok(cntlr_cfg)
}

#[derive(Clone, Default)]
pub struct ScsiDevConfig {
    /// Scsi Device id.
    pub id: String,
    /// The image file path.
    pub path_on_host: String,
    /// Serial number of the scsi device.
    pub serial: Option<String>,
    /// Scsi bus which the scsi device attaches to.
    pub bus: String,
    /// Scsi device can not do write operation.
    pub read_only: bool,
    /// If true, use direct access io.
    pub direct: bool,
    /// Async IO type.
    pub aio_type: Option<String>,
    /// Scsi four level hierarchical address(host, channel, target, lun).
    pub channel: u8,
    pub target: u8,
    pub lun: u16,
}

pub fn parse_scsi_device(vm_config: &mut VmConfig, drive_config: &str) -> Result<ScsiDevConfig> {
    let mut cmd_parser = CmdParser::new("scsi-device");
    cmd_parser
        .push("")
        .push("id")
        .push("bus")
        .push("scsi-id")
        .push("lun")
        .push("serial")
        .push("drive");

    cmd_parser.parse(drive_config)?;

    let mut scsi_dev_cfg = ScsiDevConfig::default();

    let scsi_drive = if let Some(drive) = cmd_parser.get_value::<String>("drive")? {
        drive
    } else {
        return Err(anyhow!(ConfigError::FieldIsMissing("drive", "scsi device")));
    };

    if let Some(serial) = cmd_parser.get_value::<String>("serial")? {
        scsi_dev_cfg.serial = Some(serial);
    }

    if let Some(id) = cmd_parser.get_value::<String>("id")? {
        scsi_dev_cfg.id = id;
    } else {
        return Err(anyhow!(ConfigError::FieldIsMissing("id", "scsi device")));
    }

    if let Some(bus) = cmd_parser.get_value::<String>("bus")? {
        scsi_dev_cfg.bus = bus;
    } else {
        return Err(anyhow!(ConfigError::FieldIsMissing("bus", "scsi device")));
    }

    if let Some(target) = cmd_parser.get_value::<u8>("scsi-id")? {
        if target > VIRTIO_SCSI_MAX_TARGET as u8 {
            return Err(anyhow!(ConfigError::IllegalValue(
                "scsi-id of scsi device".to_string(),
                0,
                true,
                VIRTIO_SCSI_MAX_TARGET as u64,
                true,
            )));
        }
        scsi_dev_cfg.target = target;
    }

    if let Some(lun) = cmd_parser.get_value::<u16>("lun")? {
        // Do not support Flat space addressing format(14 bits for lun) in stratovirt now.
        // We now support peripheral device addressing format(8 bits for lun).
        // So, MAX_LUN should be less than 255(2^8 - 1) temporarily.
        if lun > SUPPORT_SCSI_MAX_LUN {
            return Err(anyhow!(ConfigError::IllegalValue(
                "lun of scsi device".to_string(),
                0,
                true,
                SUPPORT_SCSI_MAX_LUN as u64,
                true,
            )));
        }
        scsi_dev_cfg.lun = lun;
    }

    if let Some(drive_arg) = &vm_config.drives.remove(&scsi_drive) {
        scsi_dev_cfg.path_on_host = drive_arg.path_on_host.clone();
        scsi_dev_cfg.read_only = drive_arg.read_only;
        scsi_dev_cfg.direct = drive_arg.direct;
        scsi_dev_cfg.aio_type = drive_arg.aio.clone();
    }

    Ok(scsi_dev_cfg)
}
