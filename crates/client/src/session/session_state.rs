// OPCUA for Rust
// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2017-2022 Adam Lock

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
    u32,
};

use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::{
    callbacks::{OnConnectionStatusChange, OnSessionClosed},
    message_queue,
    prelude::Session,
    process_unexpected_response,
    subscription_state::SubscriptionState,
};

use opcua_core::{
    comms::secure_channel::SecureChannel,
    crypto::SecurityPolicy,
    handle::Handle,
    supported_message::SupportedMessage,
    types::{status_code::StatusCode, *},
};

#[derive(Default, Copy, Clone, PartialEq, Debug, Eq)]
pub enum ConnectionState {
    #[default]
    /// No connect has been made yet
    NotStarted,
    /// Connecting
    Connecting,
    /// Connection success
    Connected,
    // Waiting for ACK from the server
    WaitingForAck,
    // Connection is running
    Processing,
    // Connection is finished, possibly after an error
    Finished(StatusCode),
}

impl ConnectionState {
    pub const fn is_connected(&self) -> bool {
        !matches!(
            self,
            ConnectionState::NotStarted
                | ConnectionState::Connecting
                | ConnectionState::Finished(_)
        )
    }
    pub const fn is_finished(&self) -> bool {
        matches!(self, ConnectionState::Finished(_))
    }
}

lazy_static! {
    static ref NEXT_SESSION_ID: AtomicU32 = AtomicU32::new(1);
}

/// Session's state indicates connection status, negotiated times and sizes,
/// and security tokens.
pub(crate) struct SessionState {
    /// A unique identifier for the session, this is NOT the session id assigned after a session is created
    pub id: u32,
    /// Time offset between the client and the server.
    client_offset: Duration,
    /// Ignore clock skew between the client and the server.
    pub ignore_clock_skew: bool,
    /// Secure channel information
    pub secure_channel: SecureChannel, // TODO: move out of this struct
    /// The request timeout is how long the session will wait from sending a request expecting a response
    /// if no response is received the client will terminate.
    request_timeout: Duration,
    /// Size of the send buffer
    pub send_buffer_size: usize,
    /// Size of the
    pub receive_buffer_size: usize,
    /// Maximum message size
    pub max_message_size: usize,
    /// Maximum chunk size
    pub max_chunk_count: usize,
    /// The session's id assigned after a connection and used for diagnostic info
    pub session_id: NodeId,
    /// The session authentication token, used for session activation
    authentication_token: NodeId,
    /// The next handle to assign to a request
    request_handle: Handle,
    /// Next monitored item client side handle
    pub monitored_item_handle: Handle,
    /// Subscription acknowledgements pending for send
    subscription_acknowledgements: Vec<SubscriptionAcknowledgement>,
    /// Subscription state
    pub subscription_state: SubscriptionState,
    /// Connection closed callback
    session_closed_callback: Option<Box<dyn OnSessionClosed + Send + Sync + 'static>>,
    /// Connection status callback
    connection_status_callback: Option<Box<dyn OnConnectionStatusChange + Send + Sync + 'static>>,
}

impl OnSessionClosed for SessionState {
    fn on_session_closed(&mut self, status_code: StatusCode) {
        debug!("Session was closed with status = {}", status_code);
        if let Some(ref mut session_closed_callback) = self.session_closed_callback {
            session_closed_callback.on_session_closed(status_code);
        }
    }
}

impl SessionState {
    const FIRST_REQUEST_HANDLE: u32 = 1;
    const FIRST_MONITORED_ITEM_HANDLE: u32 = 1000;

    const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(10 * 1000);
    const SEND_BUFFER_SIZE: usize = 65535;
    const RECEIVE_BUFFER_SIZE: usize = 65535;
    const MAX_BUFFER_SIZE: usize = 65535;

    pub fn new(
        ignore_clock_skew: bool,
        secure_channel: SecureChannel,
        subscription_state: SubscriptionState,
    ) -> SessionState {
        let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        SessionState {
            id,
            client_offset: Duration::from_millis(0),
            ignore_clock_skew,
            secure_channel,
            request_timeout: Self::DEFAULT_REQUEST_TIMEOUT,
            send_buffer_size: Self::SEND_BUFFER_SIZE,
            receive_buffer_size: Self::RECEIVE_BUFFER_SIZE,
            max_message_size: Self::MAX_BUFFER_SIZE,
            max_chunk_count: constants::MAX_CHUNK_COUNT,
            request_handle: Handle::new(Self::FIRST_REQUEST_HANDLE),
            session_id: NodeId::null(),
            authentication_token: NodeId::null(),
            monitored_item_handle: Handle::new(Self::FIRST_MONITORED_ITEM_HANDLE),
            subscription_acknowledgements: Vec::new(),
            subscription_state,
            session_closed_callback: None,
            connection_status_callback: None,
        }
    }

    pub fn set_client_offset(&mut self, offset: Duration) {
        self.client_offset = self.client_offset + offset;
        debug!("Client offset set to {:?}", self.client_offset);
    }

    pub fn set_session_id(&mut self, session_id: NodeId) {
        self.session_id = session_id
    }

    pub fn session_id(&self) -> NodeId {
        self.session_id.clone()
    }

    pub fn add_subscription_acknowledgement(
        &mut self,
        subscription_acknowledgement: SubscriptionAcknowledgement,
    ) {
        self.subscription_acknowledgements
            .push(subscription_acknowledgement);
    }

    pub fn set_authentication_token(&mut self, authentication_token: NodeId) {
        self.authentication_token = authentication_token;
    }

    pub fn set_session_closed_callback<CB>(&mut self, session_closed_callback: CB)
    where
        CB: OnSessionClosed + Send + Sync + 'static,
    {
        self.session_closed_callback = Some(Box::new(session_closed_callback));
    }

    pub fn set_connection_status_callback<CB>(&mut self, connection_status_callback: CB)
    where
        CB: OnConnectionStatusChange + Send + Sync + 'static,
    {
        self.connection_status_callback = Some(Box::new(connection_status_callback));
    }

    pub(crate) fn on_connection_status_change(&mut self, connected: bool) {
        if let Some(ref mut connection_status) = self.connection_status_callback {
            connection_status.on_connection_status_change(connected);
        }
    }
}

pub(crate) async fn reset(session: &Arc<RwLock<Session>>) {
    // Clear tokens, ids etc.
    session.try_write().unwrap().session_state.session_id = NodeId::null();
    session
        .try_write()
        .unwrap()
        .session_state
        .authentication_token = NodeId::null();
    session
        .try_write()
        .unwrap()
        .session_state
        .request_handle
        .reset();
    session
        .try_write()
        .unwrap()
        .session_state
        .monitored_item_handle
        .reset();

    session
        .try_read()
        .unwrap()
        .message_queue
        .send(message_queue::Request::Clear)
        .await
        .unwrap();
}

/// Construct a request header for the session. All requests after create session are expected
/// to supply an authentication token.
pub fn make_request_header(session: &Arc<RwLock<Session>>) -> RequestHeader {
    let timestamp = DateTime::now_with_offset(
        chrono::Duration::from_std(session.try_read().unwrap().session_state.client_offset)
            .unwrap(),
    );
    let authentication_token = session
        .try_read()
        .unwrap()
        .session_state
        .authentication_token
        .clone();
    let timeout_hint = session
        .try_read()
        .unwrap()
        .session_state
        .request_timeout
        .as_millis() as u32;
    let request_handle = session
        .try_write()
        .unwrap()
        .session_state
        .request_handle
        .next();
    let return_diagnostics = DiagnosticBits::empty();

    RequestHeader {
        authentication_token,
        timestamp,
        request_handle,
        return_diagnostics,
        timeout_hint,
        ..Default::default()
    }
}

/// Synchronously sends a request. The return value is the response to the request
pub(crate) async fn send_request<T>(
    session: &Arc<RwLock<Session>>,
    request: T,
) -> Result<SupportedMessage, StatusCode>
where
    T: Into<SupportedMessage>,
{
    let (sender, receiver) = oneshot::channel();

    let msg_queue = session.try_read().unwrap().message_queue.clone();
    let request_handle = async_send_request(session, msg_queue, request, Some(sender)).await?;
    let request_timeout = session.try_read().unwrap().session_state.request_timeout;
    wait_for_sync_response(request_handle, request_timeout, receiver).await
}

/// Asynchronously sends a request. The return value is the request handle of the request
pub(crate) async fn async_send_request<T>(
    session: &Arc<RwLock<Session>>,
    message_queue: mpsc::Sender<message_queue::Request>,
    request: T,
    sender: Option<oneshot::Sender<SupportedMessage>>,
) -> Result<u32, StatusCode>
where
    T: Into<SupportedMessage>,
{
    log::debug!("async send request");
    let request = request.into();
    match request {
        SupportedMessage::OpenSecureChannelRequest(_)
        | SupportedMessage::CloseSecureChannelRequest(_) => {}
        _ => {
            log::debug!("Make sure secure channel token hasn't expired");
            let _ = ensure_secure_channel_token(&session);
        }
    }

    // TODO should error here if not connected

    log::debug!("Enqueue the request");
    let request_handle = request.request_handle();

    if let Some(sender) = sender {
        message_queue
            .send(message_queue::Request::AddRequest(request, sender))
            .await
            .unwrap();
    } else {
        message_queue
            .send(message_queue::Request::Publish(request))
            .await
            .unwrap();
    }
    Ok(request_handle)
}

/// Wait for a response with a matching request handle. If request handle is 0 then no match
/// is performed and in fact the function is expected to receive no messages except asynchronous
/// and housekeeping events from the server. A 0 handle will cause the wait to process at most
/// one async message before returning.
async fn wait_for_sync_response(
    request_handle: u32,
    request_timeout: Duration,
    receiver: oneshot::Receiver<SupportedMessage>,
) -> Result<SupportedMessage, StatusCode> {
    log::warn!("wait_for_sync_response");

    if request_handle == 0 {
        panic!("Request handle must be non zero");
    }
    // Receive messages until the one expected comes back.
    // Publish responses will be consumed silently.
    receiver.await.map_err(|_| StatusCode::BadInternalError)
    // TODO: add timeout
    // info!("Timeout waiting for response from server");
    // session
    //     .try_write().unwrap()
    //     .message_queue
    //     .request_has_timed_out(request_handle);
    // StatusCode::BadTimeout
}

/// Checks if secure channel token needs to be renewed and renews it
async fn ensure_secure_channel_token(session: &Arc<RwLock<Session>>) -> Result<(), StatusCode> {
    if session
        .try_read()
        .unwrap()
        .session_state
        .secure_channel
        .should_renew_security_token()
    {
        return issue_or_renew_secure_channel(session, SecurityTokenRequestType::Renew).await;
    }
    Ok(())
}

pub(crate) async fn issue_or_renew_secure_channel(
    session: &Arc<RwLock<Session>>,
    request_type: SecurityTokenRequestType,
) -> Result<(), StatusCode> {
    log::trace!("issue_or_renew_secure_channel({:?})", request_type);

    const REQUESTED_LIFETIME: u32 = 60000; // TODO

    let (security_mode, security_policy, client_nonce) = {
        let client_nonce = session
            .try_write()
            .unwrap()
            .session_state
            .secure_channel
            .security_policy
            .random_nonce();
        session
            .try_write()
            .unwrap()
            .session_state
            .secure_channel
            .set_local_nonce(client_nonce.as_ref());
        (
            session
                .try_read()
                .unwrap()
                .session_state
                .secure_channel
                .security_mode,
            session
                .try_read()
                .unwrap()
                .session_state
                .secure_channel
                .security_policy,
            client_nonce,
        )
    };

    log::info!("Making secure channel request");
    log::info!("security_mode = {:?}", security_mode);
    log::info!("security_policy = {:?}", security_policy);

    let requested_lifetime = REQUESTED_LIFETIME;
    let request = OpenSecureChannelRequest {
        request_header: make_request_header(&session),
        client_protocol_version: 0,
        request_type,
        security_mode,
        client_nonce,
        requested_lifetime,
    };
    let response = send_request(&session, request).await?;
    let SupportedMessage::OpenSecureChannelResponse(response) = response else {
        return Err(process_unexpected_response(response));
    };
    // Extract the security token from the response.
    let mut security_token = response.security_token.clone();

    // When ignoring clock skew, we calculate the time offset between the client and the
    // server and use that offset to compensate for the difference in time when setting
    // the timestamps in the request headers and when decoding timestamps in messages
    // received from the server.
    if session.try_read().unwrap().session_state.ignore_clock_skew
        && !response.response_header.timestamp.is_null()
    {
        let offset = response.response_header.timestamp - DateTime::now();
        // Make sure to apply the offset to the security token in the current response.
        security_token.created_at = security_token.created_at - offset;
        // Update the client offset by adding the new offset. When the secure channel is
        // renewed its already using the client offset calculated when issuing the secure
        // channel and only needs to be updated to accommodate any additional clock skew.
        session
            .try_write()
            .unwrap()
            .session_state
            .set_client_offset(offset.to_std().unwrap());
    }

    log::debug!("Setting transport's security token");

    let offset =
        chrono::Duration::from_std(session.try_read().unwrap().session_state.client_offset)
            .unwrap();

    session
        .try_write()
        .unwrap()
        .session_state
        .secure_channel
        .set_client_offset(offset);
    log::debug!("set_security_token");
    session
        .try_write()
        .unwrap()
        .session_state
        .secure_channel
        .set_security_token(security_token);

    if security_policy != SecurityPolicy::None
        && (security_mode == MessageSecurityMode::Sign
            || security_mode == MessageSecurityMode::SignAndEncrypt)
    {
        log::debug!("set_remote_nonce_from_byte_string");
        session
            .try_write()
            .unwrap()
            .session_state
            .secure_channel
            .set_remote_nonce_from_byte_string(&response.server_nonce)?;
        log::debug!("derive_keys");
        session
            .try_write()
            .unwrap()
            .session_state
            .secure_channel
            .derive_keys();
    }
    Ok(())
}

/// Sends a publish request containing acknowledgements for previous notifications.
pub async fn async_publish(session: &Arc<RwLock<Session>>) -> Result<u32, StatusCode> {
    let subscription_acknowledgements = if session
        .try_read()
        .unwrap()
        .session_state
        .subscription_acknowledgements
        .is_empty()
    {
        None
    } else {
        let subscription_acknowledgements: Vec<_> = session
            .try_write()
            .unwrap()
            .session_state
            .subscription_acknowledgements
            .drain(..)
            .collect();
        // Debug sequence nrs
        if log_enabled!(log::Level::Debug) {
            let sequence_nrs: Vec<u32> = subscription_acknowledgements
                .iter()
                .map(|ack| ack.sequence_number)
                .collect();
            log::debug!("async_publish is acknowledging subscription acknowledgements with sequence nrs {sequence_nrs:?}");
        }
        Some(subscription_acknowledgements)
    };

    let request_header = make_request_header(session);
    let request = PublishRequest {
        request_header,
        subscription_acknowledgements,
    };
    let msg_q = session.read().message_queue.clone();
    let request_handle = async_send_request(session, msg_q, request, None).await?;

    session
        .try_write()
        .unwrap()
        .session_state
        .subscription_state
        .set_last_publish_request(Instant::now());

    debug!("async_publish, request sent with handle {}", request_handle);
    Ok(request_handle)
}

/// This is the handler for asynchronous responses which are currently assumed to be publish
/// responses. It maintains the acknowledgements to be sent and sends the data change
/// notifications to the client for processing.
async fn handle_async_response(session: &Arc<RwLock<Session>>, response: SupportedMessage) {
    log::debug!("handle_async_response");
    match response {
        SupportedMessage::PublishResponse(response) => {
            log::debug!("PublishResponse");

            // Update subscriptions based on response
            // Queue acknowledgements for next request
            let notification_message = response.notification_message.clone();
            let subscription_id = response.subscription_id;

            // Queue an acknowledgement for this request (if it has data)
            if let Some(ref notification_data) = notification_message.notification_data {
                if !notification_data.is_empty() {
                    session
                        .try_write()
                        .unwrap()
                        .session_state
                        .add_subscription_acknowledgement(SubscriptionAcknowledgement {
                            subscription_id,
                            sequence_number: notification_message.sequence_number,
                        });
                }
            }

            let decoding_options = session
                .try_read()
                .unwrap()
                .session_state
                .secure_channel
                .decoding_options();

            // Process data change notifications
            if let Some((data_change_notifications, events)) =
                notification_message.notifications(&decoding_options)
            {
                log::debug!(
                    "Received notifications, data changes = {}, events = {}",
                    data_change_notifications.len(),
                    events.len()
                );
                if !data_change_notifications.is_empty() {
                    session
                        .try_write()
                        .unwrap()
                        .session_state
                        .subscription_state
                        .on_data_change(subscription_id, &data_change_notifications);
                }
                if !events.is_empty() {
                    session
                        .try_write()
                        .unwrap()
                        .session_state
                        .subscription_state
                        .on_event(subscription_id, &events);
                }
            }

            // Send another publish request
            let _ = async_publish(session).await;
        }
        SupportedMessage::ServiceFault(response) => {
            let service_result = response.response_header.service_result;
            log::debug!("Service fault received with {} error code", service_result);
            log::trace!("ServiceFault {:?}", response);

            match service_result {
                StatusCode::BadTimeout => {
                    debug!("Publish request timed out so sending another");
                    let _ = async_publish(session).await;
                }
                StatusCode::BadTooManyPublishRequests => {
                    // Turn off publish requests until server says otherwise
                    debug!("Server tells us too many publish requests so waiting for a response before resuming");
                }
                StatusCode::BadSessionClosed
                | StatusCode::BadSessionIdInvalid
                | StatusCode::BadNoSubscription
                | StatusCode::BadSubscriptionIdInvalid => session
                    .try_write()
                    .unwrap()
                    .session_state
                    .on_session_closed(service_result),
                _ => (),
            }
        }
        _ => {
            log::info!(
                "{} unhandled response",
                session.try_read().unwrap().session_state.session_id()
            );
        }
    }
}

// Process any async messages we expect to receive
pub(crate) async fn handle_publish_responses(session: &Arc<RwLock<Session>>) -> bool {
    let (tx, rx) = oneshot::channel();
    session
        .read()
        .message_queue
        .send(message_queue::Request::GetResponses(tx))
        .await
        .unwrap();
    let responses = rx.await.unwrap();
    if responses.is_empty() {
        return false;
    }
    log::debug!("Processing {} async messages", responses.len());
    for response in responses {
        handle_async_response(session, response).await;
    }
    true
}
