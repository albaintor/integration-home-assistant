// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

use std::collections::{HashMap, HashSet};
use std::io::{Error, ErrorKind};
use std::time::{Duration, SystemTime};

use actix::prelude::{Actor, Context, Handler, Recipient};
use actix::{ActorFutureExt, Addr, AsyncContext, MessageResult, ResponseActFuture, WrapFuture};
use futures::StreamExt;
use log::{debug, error, info, warn};
use serde_json::json;
use strum::EnumMessage;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use uc_api::ws::{EventCategory, WsError, WsMessage};
use uc_api::{ApiVersion, DeviceState, EntityCommand, SubscribeEvents};

use crate::client::messages::{
    AvailableEntities, CallService, Close, ConnectionEvent, ConnectionState, EntityEvent, GetStates,
};
use crate::client::HomeAssistantClient;
use crate::configuration::HomeAssistantSettings;
use crate::messages::{
    Connect, Disconnect, GetDeviceState, NewR2Session, R2Event, R2EventMsg, R2Request,
    R2RequestMsg, R2SessionDisconnect, SendWsMessage,
};
use crate::websocket::new_websocket_client;

struct R2Session {
    recipient: Recipient<SendWsMessage>,
    standby: bool,
    subscribed_entities: HashSet<String>,
    /// HomeAssistant connection mode: true = connect (& reconnect), false = disconnect (& don't reconnect)
    ha_connect: bool,
    get_available_entities_id: Option<u32>,
}

impl R2Session {
    fn new(recipient: Recipient<SendWsMessage>) -> Self {
        Self {
            recipient,
            standby: false,
            subscribed_entities: Default::default(),
            ha_connect: false,
            get_available_entities_id: None,
        }
    }
}

pub struct Controller {
    // TODO use actor address instead? Recipient is generic but only allows one specific message
    /// Active Remote Two WebSocket sessions
    sessions: HashMap<String, R2Session>,
    /// Home Assistant connection state
    device_state: DeviceState,
    settings: HomeAssistantSettings,
    /// WebSocket client
    // creating an expensive client is sufficient once per process and can be used to create multiple connections
    ws_client: awc::Client,
    /// HomeAssistant client actor
    ha_client: Option<Addr<HomeAssistantClient>>,
    ha_reconnect_duration: Duration,
    ha_reconnect_attempt: u16,
}

impl Controller {
    pub fn new(settings: HomeAssistantSettings) -> Self {
        Self {
            sessions: Default::default(),
            device_state: DeviceState::Disconnected,
            ws_client: new_websocket_client(
                Duration::from_secs(settings.connection_timeout as u64),
                settings.url.to_lowercase().starts_with("wss"),
            ),
            ha_reconnect_duration: settings.reconnect.duration,
            settings,
            ha_client: None,
            ha_reconnect_attempt: 0,
        }
    }

    fn send_message(&self, message: WsMessage, ws_id: &str) {
        if let Some(session) = self.sessions.get(ws_id) {
            if session.standby {
                debug!("Remote is in standby, not sending message: {:?}", message);
                // TODO queue entity update events?
                return;
            }
            // TODO use send instead?
            // TODO error handling
            let _ = session.recipient.do_send(SendWsMessage(message));
        } else {
            warn!("attempting to send message but couldn't find user id.");
        }
    }

    fn send_device_state(&self, ws_id: &str) {
        self.send_message(
            WsMessage::event(
                "device_state",
                Some(EventCategory::Device),
                to_rfc3339(SystemTime::now()),
                json!({ "state": self.device_state }),
            ),
            ws_id,
        );
    }

    fn broadcast_device_state(&self) {
        for session in self.sessions.keys() {
            // TODO filter out remotes which don't require an active HA connection?
            self.send_device_state(session);
        }
    }

    fn set_device_state(&mut self, state: DeviceState) {
        self.device_state = state;
        self.broadcast_device_state();
    }

    fn increment_reconnect_timeout(&mut self) {
        let new_timeout = Duration::from_millis(
            (self.ha_reconnect_duration.as_millis() as f32 * self.settings.reconnect.backoff_factor)
                as u64,
        );

        self.ha_reconnect_duration = if new_timeout.gt(&self.settings.reconnect.duration_max) {
            self.settings.reconnect.duration_max
        } else {
            new_timeout
        };
        info!(
            "New reconnect timeout: {}",
            self.ha_reconnect_duration.as_millis()
        )
    }
}

impl Actor for Controller {
    type Context = Context<Self>;
}

impl Handler<ConnectionEvent> for Controller {
    type Result = ();

    fn handle(&mut self, msg: ConnectionEvent, ctx: &mut Self::Context) -> Self::Result {
        match msg.state {
            ConnectionState::AuthenticationFailed => {
                // error state prevents auto-reconnect in upcoming Closed event
                self.set_device_state(DeviceState::Error);
            }
            ConnectionState::Connected => {
                self.set_device_state(DeviceState::Connected);
            }
            ConnectionState::Closed => {
                info!("HA client disconnected: {}", msg.client_id);
                self.ha_client = None;

                if matches!(
                    self.device_state,
                    DeviceState::Connecting | DeviceState::Connected
                ) {
                    info!("Start reconnecting to HA: {}", msg.client_id);
                    // TODO add incremental delay logic as in the connection establish process,
                    // otherwise there's an infinite connect -> close -> connect loop without abort
                    // for certain errors (e.g. when we forget to increment the message id).
                    self.set_device_state(DeviceState::Connecting);

                    ctx.notify(Connect {});
                }
            }
        };
    }
}

impl Handler<EntityEvent> for Controller {
    type Result = ();

    fn handle(&mut self, msg: EntityEvent, _ctx: &mut Self::Context) -> Self::Result {
        // TODO keep an entity subscription per remote session and filter out non-subscribed remotes?
        if let Ok(msg_data) = serde_json::to_value(msg.entity_change) {
            for session in self.sessions.keys() {
                self.send_message(
                    WsMessage::event(
                        "entity_change",
                        Some(EventCategory::Entity),
                        to_rfc3339(SystemTime::now()),
                        msg_data.clone(),
                    ),
                    session,
                );
            }
        }
    }
}

impl Handler<AvailableEntities> for Controller {
    type Result = ();

    fn handle(&mut self, msg: AvailableEntities, _ctx: &mut Self::Context) -> Self::Result {
        // TODO just a quick implementation. Implement caching and request filter!
        if let Ok(msg_data) = serde_json::to_value(msg.entities) {
            for (ws_id, session) in self.sessions.iter_mut() {
                if let Some(id) = session.get_available_entities_id {
                    if session.standby {
                        debug!(
                            "[{}] Remote is in standby, not sending message: available_entities",
                            ws_id
                        );
                        continue;
                    }
                    match session
                        .recipient
                        .try_send(SendWsMessage(WsMessage::response(
                            id,
                            "available_entities",
                            msg_data.clone(),
                        ))) {
                        Ok(_) => session.get_available_entities_id = None,
                        Err(e) => error!("[{}] Error sending available_entities: {:?}", ws_id, e),
                    }
                }
            }
        }
    }
}

impl Handler<Disconnect> for Controller {
    type Result = ();

    fn handle(&mut self, _msg: Disconnect, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(addr) = self.ha_client.as_ref() {
            addr.do_send(Close::default());
        }
    }
}

impl Handler<Connect> for Controller {
    type Result = ResponseActFuture<Self, Result<(), Error>>;

    fn handle(&mut self, _msg: Connect, ctx: &mut Self::Context) -> Self::Result {
        // TODO check if already connected

        let ws_request = self.ws_client.ws(&self.settings.url);
        let url = self.settings.url.clone();
        let token = self.settings.token.clone();
        let client_address = ctx.address();
        let heartbeat = self.settings.heartbeat.clone();

        Box::pin(
            async move {
                debug!("Connecting to: {}", url);

                let (response, framed) = match ws_request.connect().await {
                    Ok((r, f)) => (r, f),
                    Err(e) => {
                        warn!("Could not connect to {}: {:?}", url, e);
                        return Err(Error::new(ErrorKind::Other, e.to_string()));
                    }
                };
                info!("Connected to: {} - {:?}", url, response);

                let id = url.replace("/api/websocket", "");
                let (sink, stream) = framed.split();
                let addr =
                    HomeAssistantClient::start(id, client_address, token, sink, stream, heartbeat);

                Ok(addr)
            }
            .into_actor(self) // converts future to ActorFuture
            .map(move |result, act, ctx| {
                match result {
                    Ok(addr) => {
                        debug!("Successfully connected to: {}", act.settings.url);
                        act.ha_client = Some(addr);
                        act.ha_reconnect_duration = act.settings.reconnect.duration;
                        act.ha_reconnect_attempt = 0;
                        Ok(())
                    }
                    Err(e) => {
                        // TODO quick and dirty: simply send Connect message as simple reconnect mechanism. Needs to be refined!
                        if act.device_state != DeviceState::Disconnected {
                            act.ha_reconnect_attempt += 1;
                            if act.ha_reconnect_attempt > act.settings.reconnect.attempts {
                                info!(
                                    "Max reconnect attempts reached ({}). Giving up!",
                                    act.settings.reconnect.attempts
                                );
                                act.device_state = DeviceState::Error;
                                act.broadcast_device_state();
                            } else {
                                ctx.notify_later(Connect {}, act.ha_reconnect_duration);
                                act.increment_reconnect_timeout();
                            }
                        }
                        Err(e)
                    }
                }
            }),
        )
    }
}

impl Handler<NewR2Session> for Controller {
    type Result = ();

    fn handle(&mut self, msg: NewR2Session, _: &mut Context<Self>) -> Self::Result {
        self.sessions
            .insert(msg.id.clone(), R2Session::new(msg.addr));

        self.send_device_state(&msg.id);
    }
}

impl Handler<R2SessionDisconnect> for Controller {
    type Result = ();

    fn handle(&mut self, msg: R2SessionDisconnect, _: &mut Context<Self>) {
        if self.sessions.remove(&msg.id).is_some() {
            // TODO
        }
    }
}

impl Handler<GetDeviceState> for Controller {
    type Result = MessageResult<GetDeviceState>;

    fn handle(&mut self, _: GetDeviceState, _: &mut Self::Context) -> Self::Result {
        MessageResult(self.device_state.clone())
    }
}

impl Handler<R2RequestMsg> for Controller {
    type Result = ();

    fn handle(&mut self, msg: R2RequestMsg, _ctx: &mut Self::Context) -> Self::Result {
        // extra safety: if we get a request, the remote is certainly not in standby mode
        if let Some(session) = self.sessions.get_mut(&msg.ws_id) {
            session.standby = false;
        }

        let resp_msg = msg.request.get_message().unwrap();
        match msg.request {
            R2Request::GetDriverVersion => {
                self.send_message(
                    WsMessage::response(
                        msg.req_id,
                        resp_msg,
                        // TODO make a global var?
                        // TODO Read versions from project / during build.
                        ApiVersion {
                            api: "0.4.0".to_string(),
                            integration: "0.1.0".to_string(),
                        },
                    ),
                    &msg.ws_id,
                );
                return;
            }
            R2Request::GetDeviceState => {
                self.send_message(
                    WsMessage::event(
                        resp_msg,
                        Some(EventCategory::Device),
                        to_rfc3339(SystemTime::now()),
                        json!({ "state": self.device_state }),
                    ),
                    &msg.ws_id,
                );
                return;
            }
            R2Request::SetupDevice => {}
            R2Request::GetAvailableEntities => {
                if let Some(session) = self.sessions.get_mut(&msg.ws_id) {
                    session.get_available_entities_id = Some(msg.req_id);
                }

                // FIXME proof of concept only. TODO add caching and maybe a "force retrieve flag"
                if let Some(addr) = self.ha_client.as_ref() {
                    addr.do_send(GetStates);
                }
                return;
            }
            R2Request::SubscribeEvents => {
                if let Some(msg_data) = msg.msg_data {
                    let result: serde_json::Result<SubscribeEvents> =
                        serde_json::from_value(msg_data);
                    if let Ok(subscribe) = result {
                        if let Some(session) = self.sessions.get_mut(&msg.ws_id) {
                            session
                                .subscribed_entities
                                .extend(subscribe.entity_ids.into_iter());
                        }
                    } else {
                        warn!(
                            "[{}] Invalid subscribe_events payload: {:?}",
                            msg.ws_id, result
                        )
                    }
                }
                return;
            }
            R2Request::UnsubscribeEvents => {
                if let Some(msg_data) = msg.msg_data {
                    let result: serde_json::Result<SubscribeEvents> =
                        serde_json::from_value(msg_data);
                    if let Ok(unsubscribe) = result {
                        if let Some(session) = self.sessions.get_mut(&msg.ws_id) {
                            for i in unsubscribe.entity_ids {
                                session.subscribed_entities.remove(&i);
                            }
                        }
                    } else {
                        warn!(
                            "[{}] Invalid unsubscribe_events payload: {:?}",
                            msg.ws_id, result
                        )
                    }
                }
                return;
            }
            R2Request::GetEntityStates => {}
            R2Request::EntityCommand => {
                let msg_data = match msg.msg_data {
                    None => {
                        warn!("Missing msg_data in entity command");
                        return;
                    }
                    Some(v) => v,
                };
                let command = match serde_json::from_value::<EntityCommand>(msg_data) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        warn!("Invalid entity command: {:?}", e);
                        return;
                    }
                };
                if let Some(addr) = self.ha_client.as_ref() {
                    addr.do_send(CallService { command });
                }
                return;
            }
        }
        info!("TODO implement {:?}", msg);
        // Not yet implemented response
        self.send_message(
            WsMessage::error(
                msg.req_id,
                501,
                WsError {
                    code: "NOT_IMPLEMENTED".to_string(),
                    message: "Not yet implemented".to_string(),
                },
            ),
            &msg.ws_id,
        );
    }
}

impl Handler<R2EventMsg> for Controller {
    type Result = ();

    fn handle(&mut self, msg: R2EventMsg, ctx: &mut Self::Context) -> Self::Result {
        let session = match self.sessions.get_mut(&msg.ws_id) {
            None => {
                error!("Session not found: {}", msg.ws_id);
                return;
            }
            Some(s) => s,
        };

        match msg.event {
            R2Event::Connect => {
                session.ha_connect = true;

                if self.device_state != DeviceState::Connected {
                    self.device_state = DeviceState::Connecting;
                    self.send_device_state(&msg.ws_id);
                    ctx.notify(Connect {});
                }
            }
            R2Event::Disconnect => {
                session.ha_connect = false;
                ctx.notify(Disconnect {});
                // this prevents automatic reconnects
                self.set_device_state(DeviceState::Disconnected);
            }
            R2Event::EnterStandby => {
                session.standby = true;
            }
            R2Event::ExitStandby => {
                session.standby = false;
                // TODO send updates
            }
            _ => info!("Unsupported event: {:?}", msg.event),
        }
    }
}

fn to_rfc3339<T>(dt: T) -> Option<String>
where
    T: Into<OffsetDateTime>,
{
    dt.into().format(&Rfc3339).ok()
}