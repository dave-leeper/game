use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::str::FromStr;

use bevy::app::PluginGroupBuilder;
use bevy::prelude::*;
use bevy::utils::Duration;
use bevy::input::touch::*;

use lightyear::_reexport::ShouldBeInterpolated;
pub use lightyear::prelude::client::*;
use lightyear::prelude::*;

use crate::protocol::Direction;
use crate::protocol::*;
use crate::shared::{color_from_id, shared_config, shared_movement_behaviour};
use crate::{shared, ClientTransports, SharedSettings};

pub struct ClientPluginGroup {
    lightyear: ClientPlugin<MyProtocol>,
}

impl ClientPluginGroup {
    pub(crate) fn new(net_config: NetConfig) -> ClientPluginGroup {
        let config = ClientConfig {
            shared: shared_config(),
            net: net_config,
            interpolation: InterpolationConfig::default()
                .with_delay(InterpolationDelay::default().with_send_interval_ratio(2.0)),
            ..default()
        };
        let plugin_config = PluginConfig::new(config, protocol());
        ClientPluginGroup {
            lightyear: ClientPlugin::new(plugin_config),
        }
    }
}

pub struct SteamConfig {
    pub server_addr: SocketAddr,
    pub app_id: u32,
}
impl Default for SteamConfig {
    fn default() -> Self {
        Self {
            server_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 27015)),
            // app id of the public Space Wars demo app
            app_id: 480,
        }
    }
}

impl PluginGroup for ClientPluginGroup {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::start::<Self>()
            .add(self.lightyear)
            .add(ExampleClientPlugin)
            .add(shared::SharedPlugin)
    }
}

pub struct ExampleClientPlugin;

impl Plugin for ExampleClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, init);
        app.add_systems(PreUpdate, spawn_cursor.after(MainSet::ReceiveFlush));
        // Inputs need to be buffered in the `FixedPreUpdate` schedule
        app.add_systems(
            FixedPreUpdate,
            buffer_input.in_set(InputSystemSet::BufferInputs),
        );
        // all actions related-system that can be rolled back should be in the `FixedUpdate` schedule
        app.add_systems(FixedUpdate, (player_movement, delete_player));
        app.add_systems(
            Update,
            (
                cursor_movement,
                receive_message,
                send_message,
                spawn_player,
                handle_predicted_spawn,
                handle_interpolated_spawn,
                touch_event_system,
            ),
        );
    }
}

// Startup system for the client
pub(crate) fn init(mut commands: Commands, mut client: ResMut<ClientConnection>) {
    commands.spawn(Camera2dBundle::default());
    let _ = client.connect();
}

pub(crate) fn spawn_cursor(mut commands: Commands, metadata: Res<GlobalMetadata>) {
    // the `GlobalMetadata` resource holds metadata related to the client
    // once the connection is established.
    if metadata.is_changed() {
        if let Some(client_id) = metadata.client_id {
            commands.spawn(TextBundle::from_section(
                format!("Client {}", client_id),
                TextStyle {
                    font_size: 30.0,
                    color: Color::WHITE,
                    ..default()
                },
            ));
            // spawn a local cursor which will be replicated to other clients, but remain client-authoritative.
            commands.spawn(CursorBundle::new(
                client_id,
                Vec2::ZERO,
                color_from_id(client_id),
            ));
        }
    }
}

// System that reads from peripherals and adds inputs to the buffer
pub(crate) fn buffer_input(
    tick_manager: Res<TickManager>,
    mut connection_manager: ResMut<ClientConnectionManager>,
    keypress: Res<ButtonInput<KeyCode>>,
) {
    let tick = tick_manager.tick();
    let mut direction = Direction {
        up: false,
        down: false,
        left: false,
        right: false,
    };
    if keypress.pressed(KeyCode::KeyW) || keypress.pressed(KeyCode::ArrowUp) {
        direction.up = true;
    }
    if keypress.pressed(KeyCode::KeyS) || keypress.pressed(KeyCode::ArrowDown) {
        direction.down = true;
    }
    if keypress.pressed(KeyCode::KeyA) || keypress.pressed(KeyCode::ArrowLeft) {
        direction.left = true;
    }
    if keypress.pressed(KeyCode::KeyD) || keypress.pressed(KeyCode::ArrowRight) {
        direction.right = true;
    }
    if !direction.is_none() {
        return connection_manager.add_input(Inputs::Direction(direction), tick);
    }
    if keypress.pressed(KeyCode::KeyK) {
        // currently, directions is an enum and we can only add one input per tick
        return connection_manager.add_input(Inputs::Delete, tick);
    }
    if keypress.pressed(KeyCode::Space) {
        return connection_manager.add_input(Inputs::Spawn, tick);
    }
    return connection_manager.add_input(Inputs::None, tick);
}

// The client input only gets applied to predicted entities that we own
// This works because we only predict the user's controlled entity.
// If we were predicting more entities, we would have to only apply movement to the player owned one.
fn player_movement(
    mut position_query: Query<&mut PlayerPosition, With<Predicted>>,
    // InputEvent is a special case: we get an event for every fixed-update system run instead of every frame!
    mut input_reader: EventReader<InputEvent<Inputs>>,
) {
    if <Components as SyncMetadata<PlayerPosition>>::mode() != ComponentSyncMode::Full {
        return;
    }
    for input in input_reader.read() {
        if let Some(input) = input.input() {
            for position in position_query.iter_mut() {
                // NOTE: be careful to directly pass Mut<PlayerPosition>
                // getting a mutable reference triggers change detection, unless you use `as_deref_mut()`
                shared_movement_behaviour(position, input);
            }
        }
    }
}

/// Spawn a player when the space command is pressed
fn spawn_player(
    mut commands: Commands,
    mut input_reader: EventReader<InputEvent<Inputs>>,
    metadata: Res<GlobalMetadata>,
    players: Query<&PlayerId, With<PlayerPosition>>,
) {
    // return early if we still don't have access to the client id
    let Some(client_id) = metadata.client_id else {
        return;
    };

    // do not spawn a new player if we already have one
    for player_id in players.iter() {
        if player_id.0 == client_id {
            return;
        }
    }
    for input in input_reader.read() {
        if let Some(input) = input.input() {
            match input {
                Inputs::Spawn => {
                    debug!("got spawn input");
                    commands.spawn((
                        PlayerBundle::new(client_id, Vec2::ZERO, color_from_id(client_id)),
                        // IMPORTANT: this lets the server know that the entity is pre-predicted
                        // when the server replicates this entity; we will get a Confirmed entity which will use this entity
                        // as the Predicted version
                        ShouldBePredicted::default(),
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Delete the predicted player when the space command is pressed
fn delete_player(
    mut commands: Commands,
    mut input_reader: EventReader<InputEvent<Inputs>>,
    metadata: Res<GlobalMetadata>,
    players: Query<
        (Entity, &PlayerId),
        (
            With<PlayerPosition>,
            Without<Confirmed>,
            Without<Interpolated>,
        ),
    >,
) {
    // return early if we still don't have access to the client id
    let Some(client_id) = metadata.client_id else {
        return;
    };

    for input in input_reader.read() {
        if let Some(input) = input.input() {
            match input {
                Inputs::Delete => {
                    for (entity, player_id) in players.iter() {
                        if player_id.0 == client_id {
                            if let Some(mut entity_mut) = commands.get_entity(entity) {
                                // we need to use this special function to despawn prediction entity
                                // the reason is that we actually keep the entity around for a while,
                                // in case we need to re-store it for rollback
                                entity_mut.prediction_despawn::<MyProtocol>();
                                debug!("Despawning the predicted/pre-predicted player because we received player action!");
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn touch_event_system(mut touch_events: EventReader<TouchInput>) {
    for event in touch_events.read() {
        info!("{:?}", event);
    }
}

// Adjust the movement of the cursor entity based on the mouse position
fn cursor_movement(
    metadata: Res<GlobalMetadata>,
    window_query: Query<&Window>,
    mut cursor_query: Query<
        (&mut CursorPosition, &PlayerId),
        (Without<Confirmed>, Without<Interpolated>),
    >,
) {
    // return early if we still don't have access to the client id
    let Some(client_id) = metadata.client_id else {
        return;
    };

    for (mut cursor_position, player_id) in cursor_query.iter_mut() {
        if player_id.0 != client_id {
            return;
        }
        if let Ok(window) = window_query.get_single() {
            if let Some(mouse_position) = window_relative_mouse_position(window) {
                // only update the cursor if it's changed
                cursor_position.set_if_neq(CursorPosition(mouse_position));
            }
        }
    }
}

// Get the cursor position relative to the window
fn window_relative_mouse_position(window: &Window) -> Option<Vec2> {
    let Some(cursor_pos) = window.cursor_position() else {
        return None;
    };

    Some(Vec2::new(
        cursor_pos.x - (window.width() / 2.0),
        (cursor_pos.y - (window.height() / 2.0)) * -1.0,
    ))
}

// System to receive messages on the client
pub(crate) fn receive_message(mut reader: EventReader<MessageEvent<Message1>>) {
    for event in reader.read() {
        info!("Received message: {:?}", event.message());
    }
}

/// Send messages from server to clients
pub(crate) fn send_message(
    mut client: ResMut<ClientConnectionManager>,
    input: Res<ButtonInput<KeyCode>>,
) {
    if input.pressed(KeyCode::KeyM) {
        let message = Message1(5);
        info!("Send message: {:?}", message);
        // the message will be re-broadcasted by the server to all clients
        client
            .send_message_to_target::<Channel1, Message1>(Message1(5), NetworkTarget::All)
            .unwrap_or_else(|e| {
                error!("Failed to send message: {:?}", e);
            });
    }
}

// When the predicted copy of the client-owned entity is spawned, do stuff
// - assign it a different saturation
// - keep track of it in the Global resource
pub(crate) fn handle_predicted_spawn(mut predicted: Query<&mut PlayerColor, Added<Predicted>>) {
    for mut color in predicted.iter_mut() {
        color.0.set_s(0.4);
    }
}

// When the predicted copy of the client-owned entity is spawned, do stuff
// - assign it a different saturation
// - keep track of it in the Global resource
pub(crate) fn handle_interpolated_spawn(
    mut interpolated: Query<&mut PlayerColor, Added<Interpolated>>,
) {
    for mut color in interpolated.iter_mut() {
        color.0.set_s(0.1);
    }
}