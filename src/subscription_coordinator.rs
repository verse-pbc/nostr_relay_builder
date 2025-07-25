//! Subscription coordinator for handling REQ messages and historical queries
//!
//! This module replaces the actor-based subscription_service with a simpler
//! coordinator that integrates with the SubscriptionRegistry for live events.

use crate::database::RelayDatabase;
use crate::error::Error;
use crate::metrics::SubscriptionMetricsHandler;
use crate::subscription_registry::{EventDistributor, SubscriptionRegistry};
use flume;
use nostr_lmdb::Scope;
use nostr_sdk::prelude::*;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};
use websocket_builder::MessageSender;

#[derive(Debug)]
pub enum ResponseHandler {
    Oneshot(oneshot::Sender<Result<(), crate::error::Error>>),
    MessageSender(MessageSender<RelayMessage<'static>>),
}

/// Commands that can be executed against the database
#[derive(Debug)]
pub enum StoreCommand {
    /// Save an unsigned event to the database
    SaveUnsignedEvent(
        UnsignedEvent,
        Scope,
        Option<oneshot::Sender<Result<Option<Self>, crate::error::Error>>>,
    ),
    /// Save a signed event to the database
    SaveSignedEvent(Box<Event>, Scope, Option<ResponseHandler>),
    /// Delete events matching the filter from the database
    DeleteEvents(
        Filter,
        Scope,
        Option<oneshot::Sender<Result<(), crate::error::Error>>>,
    ),
}

impl StoreCommand {
    /// Get the scope for this store command
    pub fn subdomain_scope(&self) -> &Scope {
        match self {
            StoreCommand::SaveSignedEvent(_, scope, _) => scope,
            StoreCommand::SaveUnsignedEvent(_, scope, _) => scope,
            StoreCommand::DeleteEvents(_, scope, _) => scope,
        }
    }

    /// Check if this command contains a replaceable event
    pub fn is_replaceable(&self) -> bool {
        match self {
            StoreCommand::SaveUnsignedEvent(event, _, _) => {
                event.kind.is_replaceable() || event.kind.is_addressable()
            }
            StoreCommand::SaveSignedEvent(event, _, _) => {
                event.kind.is_replaceable() || event.kind.is_addressable()
            }
            StoreCommand::DeleteEvents(_, _, _) => false,
        }
    }

    pub fn set_message_sender(
        &mut self,
        message_sender: MessageSender<RelayMessage<'static>>,
    ) -> Result<(), Error> {
        match self {
            StoreCommand::SaveSignedEvent(_, _, ref mut handler) => {
                *handler = Some(ResponseHandler::MessageSender(message_sender.clone()));
                Ok(())
            }
            _ => Err(Error::internal(
                "set_message_sender called with non-SaveSignedEvent command",
            )),
        }
    }
}

/// Implement conversion from (Event, Scope) tuple to StoreCommand
impl From<(Event, Scope)> for StoreCommand {
    fn from((event, scope): (Event, Scope)) -> Self {
        StoreCommand::SaveSignedEvent(Box::new(event), scope, None)
    }
}

/// Implement conversion from (Box<Event>, Scope) tuple to StoreCommand
impl From<(Box<Event>, Scope)> for StoreCommand {
    fn from((event, scope): (Box<Event>, Scope)) -> Self {
        StoreCommand::SaveSignedEvent(event, scope, None)
    }
}

/// Buffer for replaceable events to ensure only the latest per (pubkey, kind, scope) survives
struct ReplaceableEventsBuffer {
    buffer: std::collections::HashMap<(PublicKey, Kind, Scope), UnsignedEvent>,
    sender: flume::Sender<(UnsignedEvent, Scope)>,
    receiver: Option<flume::Receiver<(UnsignedEvent, Scope)>>,
}

impl ReplaceableEventsBuffer {
    pub fn new() -> Self {
        let (sender, receiver) = flume::bounded(10_000);
        Self {
            buffer: std::collections::HashMap::new(),
            sender,
            receiver: Some(receiver),
        }
    }

    pub fn get_sender(&self) -> flume::Sender<(UnsignedEvent, Scope)> {
        self.sender.clone()
    }

    pub fn insert(&mut self, event: UnsignedEvent, scope: Scope) {
        if !event.kind.is_replaceable() && !event.kind.is_addressable() {
            debug!(
                "Skipping non-replaceable/non-addressable event kind {} for buffering",
                event.kind
            );
            return;
        }

        let key = (event.pubkey, event.kind, scope);
        self.buffer.insert(key, event);
    }

    #[allow(dead_code)] // Used in flush method
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub async fn flush(
        &mut self,
        database: &Arc<RelayDatabase>,
        crypto_helper: &crate::crypto_helper::CryptoHelper,
    ) {
        if self.buffer.is_empty() {
            return;
        }

        debug!("Flushing {} replaceable events", self.buffer.len());

        // Collect all events to sign in a batch
        let events_to_sign: Vec<(UnsignedEvent, Scope)> = self
            .buffer
            .drain()
            .map(|((_, _, scope), event)| (event, scope))
            .collect();

        // Process all events through batched signing
        for (event, scope) in events_to_sign {
            // Create a oneshot to wait for the signed result
            let (tx, rx) = tokio::sync::oneshot::channel();

            // Send for batched signing
            if let Err(e) = crypto_helper
                .sign_store_command(StoreCommand::SaveUnsignedEvent(
                    event,
                    scope.clone(),
                    Some(tx),
                ))
                .await
            {
                error!("Failed to queue replaceable event for signing: {:?}", e);
                continue;
            }

            // Wait for the signed result
            match rx.await {
                Ok(Ok(Some(signed_command))) => {
                    // Extract the signed event and save it directly
                    if let StoreCommand::SaveSignedEvent(event, scope, _) = signed_command {
                        if let Err(e) = database.save_event(&event, &scope).await {
                            error!("Failed to save replaceable event: {:?}", e);
                        }
                    }
                }
                Ok(Ok(None)) => {
                    debug!("Replaceable event signed but not saved");
                }
                Ok(Err(e)) => {
                    error!("Failed to sign replaceable event: {:?}", e);
                }
                Err(_) => {
                    error!("Signing processor dropped response channel");
                }
            }
        }
    }

    pub fn start_with_sender(
        mut self,
        database: Arc<RelayDatabase>,
        crypto_helper: crate::crypto_helper::CryptoHelper,
        cancellation_token: CancellationToken,
        task_name: String,
    ) {
        let receiver = self.receiver.take().expect("Receiver already taken");

        tokio::spawn(async move {
            debug!("{} started", task_name);

            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        debug!("{} cancelled, flushing remaining events", task_name);
                        self.flush(&database, &crypto_helper).await;
                        break;
                    }

                    event_result = receiver.recv_async() => {
                        if let Ok((event, scope)) = event_result {
                            self.insert(event, scope);
                        }
                    }

                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        self.flush(&database, &crypto_helper).await;
                    }
                }
            }
        });
    }
}

/// Coordinator for subscription management and REQ processing
#[derive(Clone)]
pub struct SubscriptionCoordinator {
    database: Arc<RelayDatabase>,
    crypto_helper: crate::crypto_helper::CryptoHelper,
    registry: Arc<SubscriptionRegistry>,
    connection_id: String,
    outgoing_sender: MessageSender<RelayMessage<'static>>,
    replaceable_event_queue: flume::Sender<(UnsignedEvent, Scope)>,
    metrics_handler: Option<Arc<dyn SubscriptionMetricsHandler>>,
    max_limit: usize,
    _connection_handle: Arc<crate::subscription_registry::ConnectionHandle>,
}

impl std::fmt::Debug for SubscriptionCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionCoordinator")
            .field("database", &self.database)
            .field("connection_id", &self.connection_id)
            .field("has_registry", &true)
            .field("metrics_handler", &self.metrics_handler.is_some())
            .field("max_limit", &self.max_limit)
            .finish()
    }
}

impl SubscriptionCoordinator {
    /// Create a new subscription coordinator
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: Arc<RelayDatabase>,
        crypto_helper: crate::crypto_helper::CryptoHelper,
        registry: Arc<SubscriptionRegistry>,
        connection_id: String,
        outgoing_sender: MessageSender<RelayMessage<'static>>,
        auth_pubkey: Option<PublicKey>,
        subdomain: Arc<Scope>,
        cancellation_token: CancellationToken,
        metrics_handler: Option<Arc<dyn SubscriptionMetricsHandler>>,
        max_limit: usize,
    ) -> Self {
        // Register this connection with the registry
        let connection_handle = registry.register_connection(
            connection_id.clone(),
            outgoing_sender.clone(),
            auth_pubkey,
            subdomain,
        );

        // Create and start the replaceable events buffer
        let buffer = ReplaceableEventsBuffer::new();
        let replaceable_event_queue = buffer.get_sender();

        buffer.start_with_sender(
            database.clone(),
            crypto_helper.clone(),
            cancellation_token,
            format!("replaceable_events_buffer_{connection_id}"),
        );

        Self {
            database,
            crypto_helper,
            registry,
            connection_id,
            outgoing_sender,
            replaceable_event_queue,
            metrics_handler,
            max_limit,
            _connection_handle: Arc::new(connection_handle),
        }
    }

    /// Add a subscription
    pub fn add_subscription(
        &self,
        subscription_id: SubscriptionId,
        filters: Vec<Filter>,
    ) -> Result<(), Error> {
        self.registry
            .add_subscription(&self.connection_id, subscription_id, filters)
    }

    /// Remove a subscription
    pub fn remove_subscription(&self, subscription_id: SubscriptionId) -> Result<(), Error> {
        // Just call directly now since it's not async
        if let Err(e) = self
            .registry
            .remove_subscription(&self.connection_id, &subscription_id)
        {
            warn!("Failed to remove subscription: {:?}", e);
        }

        Ok(())
    }

    /// Save and broadcast a store command
    pub async fn save_and_broadcast(&self, command: StoreCommand) -> Result<(), Error> {
        match command {
            StoreCommand::SaveUnsignedEvent(event, scope, response_handler) => {
                // For replaceable events, queue them for buffering
                if event.kind.is_replaceable() || event.kind.is_addressable() {
                    self.replaceable_event_queue
                        .send_async((event, scope))
                        .await
                        .map_err(|e| {
                            Error::internal(format!("Failed to queue replaceable event: {e}"))
                        })?;

                    if let Some(response_handler) = response_handler {
                        let _ = response_handler.send(Ok(None));
                    }

                    return Ok(());
                }

                let (tx, rx) = tokio::sync::oneshot::channel();
                // Send to crypto helper for batched signing
                // The crypto helper will sign and send the response through the oneshot
                self.crypto_helper
                    .sign_store_command(StoreCommand::SaveUnsignedEvent(event, scope, Some(tx)))
                    .await
                    .map_err(|e| Error::internal(format!("Failed to sign event: {e}")))?;

                // Wait for the signed result
                match rx.await {
                    Ok(Ok(Some(signed_command))) => {
                        // Extract the signed event and save it directly
                        if let StoreCommand::SaveSignedEvent(event, scope, _) = signed_command {
                            self.database
                                .save_event(&event, &scope)
                                .await
                                .map_err(|e| {
                                    Error::internal(format!("Failed to save event: {e}"))
                                })?;
                        }
                    }
                    Ok(Ok(None)) => {
                        return Err(Error::internal("Event signed but not returned"));
                    }
                    Ok(Err(e)) => {
                        return Err(Error::internal(format!("Failed to sign event: {e}")));
                    }
                    Err(_) => {
                        return Err(Error::internal(
                            "Signing processor dropped response channel",
                        ));
                    }
                }

                if let Some(response_handler) = response_handler {
                    let _ = response_handler.send(Ok(None));
                }

                Ok(())
            }
            StoreCommand::SaveSignedEvent(event, scope, response_handler) => {
                // Save the event directly to the database
                let save_result = self
                    .database
                    .save_event(&event, &scope)
                    .await
                    .map_err(|e| Error::internal(e.to_string()));

                // Send OK response if we have a MessageSender handler
                if let Some(ResponseHandler::MessageSender(mut sender)) = response_handler {
                    let ok = save_result.is_ok();
                    let msg = if ok {
                        RelayMessage::ok(event.id, true, "")
                    } else {
                        RelayMessage::ok(
                            event.id,
                            false,
                            save_result.as_ref().unwrap_err().to_string(),
                        )
                    };
                    sender.send_bypass(msg);
                } else if let Some(ResponseHandler::Oneshot(tx)) = response_handler {
                    let _ = tx.send(
                        save_result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|_| Error::internal("Failed to save event")),
                    );
                }

                // If the save was successful, distribute the event to subscribers
                if save_result.is_ok() {
                    self.registry
                        .distribute_event(Arc::new(*event), &scope)
                        .await;
                }

                save_result
            }
            StoreCommand::DeleteEvents(filter, scope, response_handler) => {
                // Delete events directly from the database
                let delete_result = self
                    .database
                    .delete(filter, &scope)
                    .await
                    .map_err(|e| Error::internal(e.to_string()));

                // Send response if we have a handler
                if let Some(handler) = response_handler {
                    let _ = handler.send(
                        delete_result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|_| Error::internal("Failed to delete events")),
                    );
                }

                delete_result
            }
        }
    }

    /// Handle a REQ message from a client
    pub async fn handle_req(
        &self,
        subscription_id: SubscriptionId,
        filters: Vec<Filter>,
        authed_pubkey: Option<PublicKey>,
        subdomain: &Scope,
        filter_fn: impl Fn(&Event, &Scope, Option<&PublicKey>) -> bool + Send + Sync + Clone + 'static,
    ) -> Result<(), Error> {
        // Process historical events first
        self.process_historical_events(
            subscription_id.clone(),
            &filters,
            authed_pubkey,
            subdomain,
            self.outgoing_sender.clone(),
            filter_fn,
        )
        .await?;

        // Add the subscription for future events
        self.add_subscription(subscription_id, filters)?;

        Ok(())
    }

    async fn process_historical_events(
        &self,
        subscription_id: SubscriptionId,
        filters: &[Filter],
        authed_pubkey: Option<PublicKey>,
        subdomain: &Scope,
        mut sender: MessageSender<RelayMessage<'static>>,
        filter_fn: impl Fn(&Event, &Scope, Option<&PublicKey>) -> bool + Send + Sync + Clone + 'static,
    ) -> Result<(), Error> {
        // Cap filter limits based on configured max_limit
        let smallest_limit = filters
            .iter()
            .filter_map(|f| f.limit)
            .min()
            .unwrap_or(self.max_limit)
            .min(self.max_limit);

        let filters: Vec<Filter> = filters
            .iter()
            .map(|filter| filter.clone().limit(smallest_limit))
            .collect();

        let mut sent_events = HashSet::new();
        let mut total_sent = 0;
        let max_limit = filters.iter().filter_map(|f| f.limit).max().unwrap_or(0);

        // Process each filter separately
        for (filter_idx, filter) in filters.iter().enumerate() {
            // All filters have been adjusted to have a limit by this point
            let requested_limit = filter
                .limit
                .expect("Filter should have limit after adjustment");

            let mut window_filter = filter.clone();
            let mut filter_sent = 0;
            let mut last_timestamp = None;
            let mut attempts = 0;
            const MAX_ATTEMPTS: usize = 50;

            loop {
                attempts += 1;
                debug!(
                    "Pagination attempt {} for filter {} of subscription {}",
                    attempts, filter_idx, subscription_id
                );

                let events = self
                    .database
                    .query(vec![window_filter.clone()], subdomain)
                    .await
                    .map_err(|e| Error::notice(format!("Failed to fetch events: {e:?}")))?;

                if events.is_empty() {
                    debug!("No more events found for filter {}", filter_idx);
                    break;
                }

                let mut filter_events = Vec::new();
                for event in events {
                    // Skip if we've already sent this event
                    if sent_events.contains(&event.id) {
                        continue;
                    }

                    // Track oldest timestamp seen for pagination
                    let event_created_at = event.created_at;
                    if last_timestamp.is_none() || Some(event_created_at) < last_timestamp {
                        last_timestamp = Some(event_created_at);
                    }

                    if filter_fn(&event, subdomain, authed_pubkey.as_ref()) {
                        filter_events.push(event);
                    }
                }

                // Send events in correct order
                // Database always returns events in descending order (newest first)
                // For all query types, maintain descending order
                filter_events.sort_by(|a, b| b.created_at.cmp(&a.created_at));

                for event in filter_events {
                    if filter_sent >= requested_limit {
                        break;
                    }

                    sent_events.insert(event.id);
                    let msg = RelayMessage::Event {
                        subscription_id: Cow::Owned(subscription_id.clone()),
                        event: Cow::Owned(event.clone()),
                    };

                    sender.send_bypass(msg);
                    filter_sent += 1;
                    total_sent += 1;
                }

                if filter_sent >= requested_limit {
                    debug!(
                        "Reached requested limit {} for filter {}",
                        requested_limit, filter_idx
                    );
                    break;
                }

                // Prepare next window by paging backward
                if let Some(ts) = last_timestamp {
                    window_filter.until = Some(ts - 1);
                } else {
                    debug!("No valid timestamp found for next window");
                    break;
                }

                if attempts >= MAX_ATTEMPTS {
                    warn!(
                        "Pagination reached max attempts ({}) for subscription {}",
                        MAX_ATTEMPTS, subscription_id
                    );
                    break;
                }
            }
        }

        debug!(
            "Pagination complete for subscription {}: sent {} events (requested max: {})",
            subscription_id, total_sent, max_limit
        );

        // Send EOSE
        sender
            .send(RelayMessage::EndOfStoredEvents(Cow::Owned(subscription_id)))
            .map_err(|e| Error::internal(format!("Failed to send EOSE: {e:?}")))?;

        Ok(())
    }

    /// Clean up resources (called on connection drop)
    pub fn cleanup(&self) {
        debug!(
            "Cleaning up subscription coordinator for connection {}",
            self.connection_id
        );
        // The connection handle will be dropped, which will remove from registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::setup_test_with_database;
    use std::time::{Duration, Instant};
    use tokio::time::sleep;

    async fn create_test_event(
        keys: &Keys,
        timestamp: Timestamp,
        group: &str,
        content: &str,
    ) -> Event {
        let tags = vec![
            Tag::custom(TagKind::from("h"), vec![group.to_string()]),
            Tag::custom(TagKind::from("test"), vec!["pagination".to_string()]),
        ];

        EventBuilder::new(Kind::from(9), content)
            .custom_created_at(timestamp)
            .tags(tags)
            .build_with_ctx(&Instant::now(), keys.public_key())
            .sign_with_keys(keys)
            .unwrap()
    }

    fn create_test_crypto_helper() -> crate::crypto_helper::CryptoHelper {
        let test_keys = Keys::generate();
        crate::crypto_helper::CryptoHelper::new(Arc::new(test_keys))
    }

    #[tokio::test]
    async fn test_replaceable_event_buffering() {
        let buffer = ReplaceableEventsBuffer::new();
        let sender = buffer.get_sender();

        // Create a replaceable event
        let event = UnsignedEvent {
            id: None,
            pubkey: PublicKey::from_slice(&[1; 32]).unwrap(),
            created_at: Timestamp::now(),
            kind: Kind::Metadata, // Replaceable kind
            tags: Tags::new(),
            content: "test".to_string(),
        };

        // Send should succeed
        sender.send_async((event, Scope::Default)).await.unwrap();
    }

    #[tokio::test]
    async fn test_window_sliding_limit_only() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            1000, // max_limit
        );

        let base_timestamp = Timestamp::from(1700000000);

        // Create 10 events alternating between public and private groups
        for i in 0..10 {
            let timestamp = Timestamp::from(base_timestamp.as_u64() + i * 10);
            let group = if i % 2 == 0 { "public" } else { "private" };
            let event = create_test_event(&keys, timestamp, group, &format!("Event {i}")).await;
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        // Wait a bit for database to process
        sleep(Duration::from_millis(100)).await;

        // Request limit=5, but only public events should be returned
        let filter = Filter::new().kinds(vec![Kind::from(9)]).limit(5);
        let sub_id = SubscriptionId::new("test_sub");

        // Filter function that only allows public group events
        let filter_fn = |event: &Event, _scope: &Scope, _auth: Option<&PublicKey>| -> bool {
            event.tags.iter().any(|t| {
                t.as_slice().len() > 1 && t.as_slice()[0] == "h" && t.as_slice()[1] == "public"
            })
        };

        // Process the subscription
        coordinator
            .handle_req(
                sub_id.clone(),
                vec![filter],
                None,
                &Scope::Default,
                filter_fn,
            )
            .await
            .unwrap();

        // Allow some time for events to be processed
        sleep(Duration::from_millis(100)).await;

        // Collect events from receiver
        let mut received_events = Vec::new();
        let mut eose_received = false;

        while let Ok(msg) = rx.try_recv() {
            match msg.0 {
                RelayMessage::Event { event, .. } => {
                    received_events.push(event.into_owned());
                }
                RelayMessage::EndOfStoredEvents(_) => {
                    eose_received = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(eose_received, "Should receive EOSE");
        assert_eq!(
            received_events.len(),
            5,
            "Should receive exactly 5 public events through pagination"
        );

        // Verify all events are public
        for event in &received_events {
            assert!(
                event.tags.iter().any(|t| t.as_slice().len() > 1
                    && t.as_slice()[0] == "h"
                    && t.as_slice()[1] == "public"),
                "All events should be from public group"
            );
        }

        // Clean up
        cancellation_token.cancel();
    }

    #[tokio::test]
    async fn test_window_sliding_until_limit() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            1000,
        );

        let base_timestamp = Timestamp::from(1700000000);

        // Create 10 events across 100 seconds
        for i in 0..10 {
            let timestamp = Timestamp::from(base_timestamp.as_u64() + i * 10);
            let group = if i % 2 == 0 { "public" } else { "private" };
            let event = create_test_event(&keys, timestamp, group, &format!("Event {i}")).await;
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        sleep(Duration::from_millis(100)).await;

        // Request with until=80 (position 8) and limit 5
        let filter = Filter::new()
            .kinds(vec![Kind::from(9)])
            .until(Timestamp::from(base_timestamp.as_u64() + 80))
            .limit(5);

        let sub_id = SubscriptionId::new("test_sub");
        let filter_fn = |event: &Event, _scope: &Scope, _auth: Option<&PublicKey>| -> bool {
            event.tags.iter().any(|t| {
                t.as_slice().len() > 1 && t.as_slice()[0] == "h" && t.as_slice()[1] == "public"
            })
        };

        coordinator
            .handle_req(
                sub_id.clone(),
                vec![filter],
                None,
                &Scope::Default,
                filter_fn,
            )
            .await
            .unwrap();

        sleep(Duration::from_millis(100)).await;

        let mut received_events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let RelayMessage::Event { event, .. } = msg.0 {
                received_events.push(event.into_owned());
            }
        }

        // Should get public events 8, 6, 4, 2, 0 through pagination
        assert_eq!(received_events.len(), 5, "Should receive 5 public events");

        // Verify they're in reverse chronological order
        for i in 1..received_events.len() {
            assert!(
                received_events[i - 1].created_at > received_events[i].created_at,
                "Events should be in reverse chronological order"
            );
        }

        cancellation_token.cancel();
    }

    #[tokio::test]
    async fn test_window_sliding_since_limit() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            1000,
        );

        let base_timestamp = Timestamp::from(1700000000);

        // Create 10 events
        for i in 0..10 {
            let timestamp = Timestamp::from(base_timestamp.as_u64() + i * 10);
            let group = if i % 2 == 0 { "public" } else { "private" };
            let event = create_test_event(&keys, timestamp, group, &format!("Event {i}")).await;
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        sleep(Duration::from_millis(100)).await;

        // Request with since=20 and limit 5
        let filter = Filter::new()
            .kinds(vec![Kind::from(9)])
            .since(Timestamp::from(base_timestamp.as_u64() + 20))
            .limit(5);

        let sub_id = SubscriptionId::new("test_sub");
        let filter_fn = |event: &Event, _scope: &Scope, _auth: Option<&PublicKey>| -> bool {
            event.tags.iter().any(|t| {
                t.as_slice().len() > 1 && t.as_slice()[0] == "h" && t.as_slice()[1] == "public"
            })
        };

        coordinator
            .handle_req(
                sub_id.clone(),
                vec![filter],
                None,
                &Scope::Default,
                filter_fn,
            )
            .await
            .unwrap();

        sleep(Duration::from_millis(100)).await;

        let mut received_events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let RelayMessage::Event { event, .. } = msg.0 {
                received_events.push(event.into_owned());
            }
        }

        // Events are created with indices 0-9
        // Timestamps: i * 10, so: 0, 10, 20, 30, 40, 50, 60, 70, 80, 90
        // Public events are at even indices (0, 2, 4, 6, 8) with timestamps: 0, 20, 40, 60, 80
        // With since=20, we get events with timestamp >= 20
        // Public events meeting this criteria: 20, 40, 60, 80 (4 events)
        // With limit=5, pagination should find all 4 public events
        assert_eq!(
            received_events.len(),
            4,
            "Should receive 4 public events with timestamps >= 20"
        );

        // Verify they're in descending order (newest first)
        for i in 1..received_events.len() {
            assert!(
                received_events[i - 1].created_at > received_events[i].created_at,
                "Events should be in descending chronological order"
            );
        }

        // Verify all events have timestamp >= 20
        for event in &received_events {
            assert!(
                event.created_at.as_u64() >= base_timestamp.as_u64() + 20,
                "All events should have timestamp >= since filter"
            );
        }

        cancellation_token.cancel();
    }

    #[tokio::test]
    async fn test_pagination_bug_scenario() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            1000,
        );

        let base_timestamp = Timestamp::from(1700000000);

        // Create 1 old accessible event
        let event =
            create_test_event(&keys, base_timestamp, "public", "Old accessible event").await;
        database.save_event(&event, &Scope::Default).await.unwrap();

        // Create 5 newer non-accessible events
        for i in 0..5 {
            let timestamp = Timestamp::from(base_timestamp.as_u64() + 100 + i * 10);
            let event =
                create_test_event(&keys, timestamp, "private", &format!("Private {i}")).await;
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        // Wait a bit for database to process
        sleep(Duration::from_millis(100)).await;

        // Request limit=5 (will get the 5 newest events, all private)
        let filter = Filter::new().kinds(vec![Kind::from(9)]).limit(5);
        let sub_id = SubscriptionId::new("test_sub");

        // Filter function that only allows public group events
        let filter_fn = |event: &Event, _scope: &Scope, _auth: Option<&PublicKey>| -> bool {
            event.tags.iter().any(|t| {
                t.as_slice().len() > 1 && t.as_slice()[0] == "h" && t.as_slice()[1] == "public"
            })
        };

        // Process the subscription - pagination should find the old public event
        coordinator
            .handle_req(
                sub_id.clone(),
                vec![filter],
                None,
                &Scope::Default,
                filter_fn,
            )
            .await
            .unwrap();

        // Allow some time for events to be processed
        sleep(Duration::from_millis(100)).await;

        // Collect events from receiver
        let mut received_events = Vec::new();
        let mut eose_received = false;

        while let Ok(msg) = rx.try_recv() {
            match msg.0 {
                RelayMessage::Event { event, .. } => {
                    received_events.push(event.into_owned());
                }
                RelayMessage::EndOfStoredEvents(_) => {
                    eose_received = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(eose_received, "Should receive EOSE");
        assert_eq!(
            received_events.len(),
            1,
            "Should find the old accessible event through pagination"
        );
        assert_eq!(received_events[0].content, "Old accessible event");

        cancellation_token.cancel();
    }

    #[tokio::test]
    async fn test_exponential_buffer_since_until_limit() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            1000,
        );

        let base_timestamp = Timestamp::from(1700000000);

        // Create 20 events: 10 public, 10 private, interleaved
        for i in 0..20 {
            let timestamp = Timestamp::from(base_timestamp.as_u64() + i * 5);
            let group = if i % 2 == 0 { "public" } else { "private" };
            let event = create_test_event(&keys, timestamp, group, &format!("Event {i}")).await;
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        // Wait a bit for database to process
        sleep(Duration::from_millis(100)).await;

        // Request events in time window [25, 75] with limit 5
        // Events are at timestamps: 0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90, 95
        // Window [25, 75] contains: 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75
        // That's indices 5-15 inclusive (11 events total)
        // Public events (even indices): 6, 8, 10, 12, 14 (5 public events)
        let filter = Filter::new()
            .kinds(vec![Kind::from(9)])
            .since(Timestamp::from(base_timestamp.as_u64() + 25))
            .until(Timestamp::from(base_timestamp.as_u64() + 75))
            .limit(5);

        let sub_id = SubscriptionId::new("test_sub");
        let filter_fn = |event: &Event, _scope: &Scope, _auth: Option<&PublicKey>| -> bool {
            event.tags.iter().any(|t| {
                t.as_slice().len() > 1 && t.as_slice()[0] == "h" && t.as_slice()[1] == "public"
            })
        };

        // This should use the unified pagination approach
        coordinator
            .handle_req(
                sub_id.clone(),
                vec![filter],
                None,
                &Scope::Default,
                filter_fn,
            )
            .await
            .unwrap();

        sleep(Duration::from_millis(100)).await;

        let mut received_events = Vec::new();
        let mut eose_received = false;

        while let Ok(msg) = rx.try_recv() {
            match msg.0 {
                RelayMessage::Event { event, .. } => {
                    received_events.push(event.into_owned());
                }
                RelayMessage::EndOfStoredEvents(_) => {
                    eose_received = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(eose_received, "Should receive EOSE");
        assert_eq!(
            received_events.len(),
            5,
            "Should receive exactly 5 public events in the time window"
        );

        // Verify all events are public and within the time window
        for event in &received_events {
            assert!(
                event.tags.iter().any(|t| t.as_slice().len() > 1
                    && t.as_slice()[0] == "h"
                    && t.as_slice()[1] == "public"),
                "All events should be from public group"
            );

            let ts = event.created_at.as_u64();
            assert!(
                ts >= base_timestamp.as_u64() + 25 && ts <= base_timestamp.as_u64() + 75,
                "Event timestamp should be within the requested window"
            );
        }

        cancellation_token.cancel();
    }

    #[tokio::test]
    async fn test_max_limit_enforcement() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        // Create coordinator with small max_limit
        let max_limit = 10;
        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            max_limit,
        );

        // Create many events
        for i in 0..30 {
            let event = EventBuilder::text_note(format!("Event {i}"))
                .build_with_ctx(&Instant::now(), keys.public_key())
                .sign_with_keys(&keys)
                .unwrap();
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        sleep(Duration::from_millis(100)).await;

        // Request with limit higher than max_limit
        let filter = Filter::new().kinds(vec![Kind::TextNote]).limit(100);
        let sub_id = SubscriptionId::new("test_sub");
        let filter_fn = |_: &Event, _: &Scope, _: Option<&PublicKey>| true;

        coordinator
            .handle_req(
                sub_id.clone(),
                vec![filter],
                None,
                &Scope::Default,
                filter_fn,
            )
            .await
            .unwrap();

        sleep(Duration::from_millis(100)).await;

        let mut event_count = 0;
        let mut eose_received = false;

        while let Ok(msg) = rx.try_recv() {
            match msg.0 {
                RelayMessage::Event { .. } => {
                    event_count += 1;
                }
                RelayMessage::EndOfStoredEvents(_) => {
                    eose_received = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(eose_received, "Should receive EOSE");
        assert_eq!(
            event_count, max_limit,
            "Should receive exactly max_limit ({}) events even though {} were requested",
            max_limit, 100
        );

        cancellation_token.cancel();
    }

    #[tokio::test]
    async fn test_multiple_filters_smallest_limit() {
        let (_tmp_dir, database, keys) = setup_test_with_database().await;
        let (tx, rx) = flume::bounded(100);
        let registry = Arc::new(SubscriptionRegistry::new(None));
        let cancellation_token = CancellationToken::new();

        let coordinator = SubscriptionCoordinator::new(
            database.clone(),
            create_test_crypto_helper(),
            registry,
            "test_conn".to_string(),
            MessageSender::new(tx, 0),
            None,
            Arc::new(Scope::Default),
            cancellation_token.clone(),
            None,
            1000,
        );

        // Create 20 events
        for i in 0..20 {
            let event = EventBuilder::text_note(format!("Event {i}"))
                .build_with_ctx(&Instant::now(), keys.public_key())
                .sign_with_keys(&keys)
                .unwrap();
            database.save_event(&event, &Scope::Default).await.unwrap();
        }

        sleep(Duration::from_millis(100)).await;

        // Create multiple filters with different limits
        let filters = vec![
            Filter::new().kinds(vec![Kind::TextNote]).limit(50),
            Filter::new().kinds(vec![Kind::TextNote]).limit(5), // Smallest limit
            Filter::new().kinds(vec![Kind::TextNote]).limit(20),
        ];

        let sub_id = SubscriptionId::new("test_sub");
        let filter_fn = |_: &Event, _: &Scope, _: Option<&PublicKey>| true;

        coordinator
            .handle_req(sub_id.clone(), filters, None, &Scope::Default, filter_fn)
            .await
            .unwrap();

        sleep(Duration::from_millis(100)).await;

        let mut event_count = 0;
        let mut eose_received = false;

        while let Ok(msg) = rx.try_recv() {
            match msg.0 {
                RelayMessage::Event { .. } => {
                    event_count += 1;
                }
                RelayMessage::EndOfStoredEvents(_) => {
                    eose_received = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(eose_received, "Should receive EOSE");
        assert_eq!(
            event_count, 5,
            "Should receive exactly 5 events (the smallest limit among filters)"
        );

        cancellation_token.cancel();
    }
}
