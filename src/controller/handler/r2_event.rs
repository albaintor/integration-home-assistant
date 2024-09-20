// Copyright (c) 2023 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

//! Actix message handler for [R2EventMsg].

use crate::controller::handler::{AbortDriverSetup, ConnectMsg, DisconnectMsg};
use crate::controller::{Controller, OperationModeInput, R2EventMsg};
use actix::clock::sleep;
use actix::{ActorFutureExt, AsyncContext, fut, Handler, ResponseActFuture, WrapFuture};
use log::{error, info};
use std::time::Duration;
use uc_api::intg::ws::R2Event;
use uc_api::intg::DeviceState;

impl Handler<R2EventMsg> for Controller {
    type Result = ResponseActFuture<Self, ()>;

    fn handle(&mut self, msg: R2EventMsg, ctx: &mut Self::Context) -> Self::Result {
        let session = match self.sessions.get_mut(&msg.ws_id) {
            None => {
                error!("Session not found: {}", msg.ws_id);
                return Box::pin(fut::ready(()));
            }
            Some(s) => s,
        };

        match msg.event {
            R2Event::Connect => {
                if self.device_state != DeviceState::Connected {
                    if self.settings.hass.get_token() != "" {
                        session.reconfiguring = None;
                        ctx.notify(ConnectMsg::default());
                        return Box::pin(
                            async move {
                                sleep(Duration::from_secs(2)).await;
                            }
                            .into_actor(self)
                            .map(move |_, _act, _ctx| {
                                info!(
                                    "(Re)connection state after configuration change {:?}",
                                    _act.device_state
                                );
                                if _act.device_state == DeviceState::Connected {
                                    if _act
                                        .sm_consume(
                                            &msg.ws_id,
                                            &OperationModeInput::ConfigurationAvailable,
                                            _ctx,
                                        )
                                        .is_err()
                                    {
                                        error!(
                                            "Error during configuration, machine state {:?}",
                                            _act.machine.state()
                                        );
                                    }
                                }
                                _act.send_device_state(&msg.ws_id);
                            }),
                        )
                    }
                };
                // make sure client has the correct state, it might be out of sync, or not calling get_device_state
                self.send_device_state(&msg.ws_id);
                return Box::pin(fut::ready(()));
            }
            R2Event::Disconnect => {
                ctx.notify(DisconnectMsg {});
                Box::pin(fut::ready(()))
            }
            R2Event::EnterStandby => {
                session.standby = true;
                if self.settings.hass.disconnect_in_standby {
                    ctx.notify(DisconnectMsg {});
                }
                Box::pin(fut::ready(()))
            }
            R2Event::ExitStandby => {
                session.standby = false;
                if self.settings.hass.disconnect_in_standby {
                    ctx.notify(ConnectMsg::default());
                    self.send_device_state(&msg.ws_id);
                }
                Box::pin(fut::ready(()))
            }
            R2Event::AbortDriverSetup => {
                ctx.notify(AbortDriverSetup {
                    ws_id: msg.ws_id,
                    timeout: false,
                });
                Box::pin(fut::ready(()))
            }
        }
    }
}
