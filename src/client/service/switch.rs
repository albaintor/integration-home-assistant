// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

//! Switch entity specific HA service call logic.

use crate::client::messages::CallService;
use crate::errors::ServiceError;
use crate::server::SwitchCommand;

use serde_json::Value;
use std::str::FromStr;

pub(crate) fn handle_switch(msg: &CallService) -> Result<(String, Option<Value>), ServiceError> {
    let cmd = SwitchCommand::from_str(&msg.command.cmd_id)?;

    let result = match cmd {
        SwitchCommand::On => ("turn_on".to_string(), None),
        SwitchCommand::Off => ("turn_off".to_string(), None),
        SwitchCommand::Toggle => ("Toggle".to_string(), None),
    };

    Ok(result)
}
