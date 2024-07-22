/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_core::is_open_source;
use buck2_event_observer::humanized::HumanizedBytes;

use crate::subscribers::recorder::process_memory;

const BYTES_PER_GIGABYTE: u64 = 1000000000;

pub(crate) struct MemoryPressureHigh {
    pub(crate) system_total_memory: u64,
    pub(crate) process_memory: u64,
}

pub(crate) struct LowDiskSpace {
    pub(crate) total_disk_space: u64,
    pub(crate) used_disk_space: u64,
}

pub const SYSTEM_MEMORY_REMEDIATION_LINK: &str = ": https://fburl.com/buck2_mem_remediation";
pub const DISK_REMEDIATION_LINK: &str = ": https://fburl.com/buck2_disk_remediation";

pub(crate) fn system_memory_exceeded_msg(memory_pressure: &MemoryPressureHigh) -> String {
    format!(
        "High memory pressure: buck2 is using {} out of {}{}",
        HumanizedBytes::new(memory_pressure.process_memory),
        HumanizedBytes::new(memory_pressure.system_total_memory),
        if is_open_source() {
            ""
        } else {
            SYSTEM_MEMORY_REMEDIATION_LINK
        }
    )
}

pub(crate) fn low_disk_space_msg(low_disk_space: &LowDiskSpace) -> String {
    format!(
        "Low disk space: only {} remaining out of {}{}",
        HumanizedBytes::new(low_disk_space.used_disk_space),
        HumanizedBytes::new(low_disk_space.total_disk_space),
        if is_open_source() {
            ""
        } else {
            DISK_REMEDIATION_LINK
        }
    )
}

pub(crate) fn check_memory_pressure(
    last_snapshot: Option<&buck2_data::Snapshot>,
    system_info: &buck2_data::SystemInfo,
) -> Option<MemoryPressureHigh> {
    let process_memory = process_memory(last_snapshot?)?;
    let system_total_memory = system_info.system_total_memory_bytes?;
    let memory_pressure_threshold_percent = system_info.memory_pressure_threshold_percent?;
    // TODO (ezgi): one-shot commands don't record this. Prevent panick (division-by-zero) until it is fixed.
    if (process_memory * 100)
        .checked_div(system_total_memory)
        .is_some_and(|res| res >= memory_pressure_threshold_percent)
    {
        Some(MemoryPressureHigh {
            system_total_memory,
            process_memory,
        })
    } else {
        None
    }
}

pub(crate) fn check_remaining_disk_space(
    last_snapshot: Option<&buck2_data::Snapshot>,
    system_info: &buck2_data::SystemInfo,
) -> Option<LowDiskSpace> {
    let used_disk_space = last_snapshot?.used_disk_space_bytes?;
    let total_disk_space = system_info.total_disk_space_bytes?;
    let remaining_disk_space_threshold =
        system_info.remaining_disk_space_threshold_gb? * BYTES_PER_GIGABYTE;

    if total_disk_space - used_disk_space <= remaining_disk_space_threshold {
        Some(LowDiskSpace {
            total_disk_space,
            used_disk_space,
        })
    } else {
        None
    }
}
