/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io::Error;

use winapi::shared::minwindef::BOOL;
use winapi::shared::minwindef::DWORD;
use winapi::shared::minwindef::FALSE;

pub(crate) fn result_bool(ret: BOOL) -> anyhow::Result<()> {
    if ret == FALSE {
        Err(anyhow::anyhow!(Error::last_os_error()))
    } else {
        Ok(())
    }
}

pub(crate) fn result_dword(ret: DWORD) -> anyhow::Result<()> {
    if ret == DWORD::MAX {
        Err(anyhow::anyhow!(Error::last_os_error()))
    } else {
        Ok(())
    }
}
