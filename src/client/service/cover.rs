// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

//! Cover entity specific HA service call logic.

use crate::client::messages::CallService;
use crate::errors::ServiceError;
use crate::server::CoverCommand;

use serde_json::Value;
use std::str::FromStr;

pub(crate) fn handle_cover(msg: &CallService) -> Result<(String, Option<Value>), ServiceError> {
    let _cmd = CoverCommand::from_str(&msg.command.cmd_id)?;

    Err(ServiceError::NotYetImplemented)
}
