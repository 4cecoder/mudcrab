//! Plain Bevy UI for settings and control reference; independent of the eventual Skyrim skin.

use crate::{
    console::{ConsoleInputSet, ConsoleState},
    physics::{CursorCapture, MotionReset},
    settings::{
        RenderingSettings, SETTING_SPECS, SettingKind, SettingsFile, SettingsPage,
        apply_render_settings,
    },
};
use bevy::{
    input::{
        InputSystems,
        mouse::{MouseScrollUnit, MouseWheel},
    },
    input_focus::{
        FocusCause, InputFocus, InputFocusPlugin, InputFocusSystems,
        tab_navigation::{TabGroup, TabIndex, handle_tab_navigation},
    },
    prelude::*,
    render::renderer::RenderAdapterInfo,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};

#[derive(Resource)]
pub struct SettingsMenuState {
    pub open: bool,
    pub closing: bool,
    pub page: SettingsPage,
    previous_capture: CursorCapture,
    was_paused: bool,
}

impl Default for SettingsMenuState {
    fn default() -> Self {
        Self {
            open: false,
            closing: false,
            page: SettingsPage::Graphics,
            previous_capture: CursorCapture::Released,
            was_paused: false,
        }
    }
}

impl SettingsMenuState {
    pub fn blocks_gameplay(&self) -> bool {
        self.open || self.closing
    }

    fn show(
        &mut self,
        capture: &mut CursorCapture,
        time: &mut Time<Virtual>,
        motion: &mut MotionReset,
    ) {
        self.open = true;
        self.previous_capture = *capture;
        self.was_paused = time.is_paused();
        *capture = CursorCapture::Released;
        time.pause();
        motion.clear();
    }

    fn close(&mut self, capture: &mut CursorCapture, time: &mut Time<Virtual>) {
        self.open = false;
        self.closing = true;
        *capture = CursorCapture::Released;
        if !self.was_paused {
            time.unpause();
        }
    }
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SettingsUiSet;

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RenderingSettings>()
            .init_resource::<SettingsMenuState>()
            .init_resource::<CursorCapture>()
            .init_resource::<InputFocus>();
        if !app.world().contains_resource::<SettingsFile>() {
            app.insert_resource(SettingsFile::session_only());
        }
        if !app.is_plugin_added::<InputFocusPlugin>() {
            app.add_plugins(InputFocusPlugin);
        }
        app.add_systems(Startup, spawn_settings_ui)
            .add_systems(
                PreUpdate,
                menu_keyboard
                    .after(InputSystems)
                    .before(InputFocusSystems::Dispatch)
                    .before(ConsoleInputSet),
            )
            .add_systems(
                Update,
                (
                    menu_actions,
                    apply_render_settings,
                    refresh_menu,
                    scroll_menu,
                    scroll_focused_button,
                )
                    .chain()
                    .in_set(SettingsUiSet),
            );
    }
}

#[derive(Component)]
struct MenuRoot;
#[derive(Component)]
struct MenuLauncher;
#[derive(Component)]
struct PageRoot(SettingsPage);
#[derive(Component)]
struct SettingValue(usize);
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct RendererText;
#[derive(Component)]
struct CollisionText;
#[derive(Component, Clone, Copy)]
enum MenuAction {
    Open,
    Page(SettingsPage),
    Adjust(usize, i32),
    CollisionDebug,
    Save,
    Defaults,
    Resume,
}

impl MenuAction {
    fn available(self, menu: &SettingsMenuState, console_open: bool) -> bool {
        if console_open || menu.closing {
            return false;
        }
        match self {
            Self::Open => !menu.open,
            Self::Adjust(index, _) => menu.open && SETTING_SPECS[index].page == menu.page,
            Self::CollisionDebug => menu.open && menu.page == SettingsPage::Graphics,
            _ => menu.open,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn menu_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    console: Option<Res<ConsoleState>>,
    mut menu: ResMut<SettingsMenuState>,
    mut capture: ResMut<CursorCapture>,
    mut time: ResMut<Time<Virtual>>,
    mut motion: MotionReset,
    mut options: Query<&mut CursorOptions, With<PrimaryWindow>>,
    mut focus: ResMut<InputFocus>,
) {
    if menu.closing {
        menu.closing = false;
        *capture = menu.previous_capture;
    }
    if console.is_some_and(|console| console.open || console.closing) {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        if menu.open {
            menu.close(&mut capture, &mut time);
            focus.clear();
        } else {
            menu.show(&mut capture, &mut time, &mut motion);
        }
    }
    if menu.blocks_gameplay() {
        for mut cursor in &mut options {
            cursor.grab_mode = CursorGrabMode::None;
            cursor.visible = true;
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn menu_actions(
    mut menu: ResMut<SettingsMenuState>,
    mut settings: ResMut<RenderingSettings>,
    mut file: ResMut<SettingsFile>,
    mut capture: ResMut<CursorCapture>,
    mut time: ResMut<Time<Virtual>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut focus: ResMut<InputFocus>,
    mut buttons: Query<
        (
            Entity,
            Ref<Interaction>,
            &MenuAction,
            &mut BackgroundColor,
            &mut BorderColor,
        ),
        With<Button>,
    >,
    mut debug: Option<ResMut<bevy_rapier3d::prelude::DebugRenderContext>>,
    mut pages: Query<&mut ScrollPosition, With<PageRoot>>,
    mut motion: MotionReset,
    console: Option<Res<ConsoleState>>,
) {
    let keyboard_activation = keys.just_pressed(KeyCode::Enter)
        || keys.just_pressed(KeyCode::NumpadEnter)
        || keys.just_pressed(KeyCode::Space);
    let mut actions = Vec::new();
    for (entity, interaction, action, mut background, mut border) in &mut buttons {
        if !action.available(&menu, console.as_ref().is_some_and(|c| c.open || c.closing)) {
            continue;
        }
        let focused = focus.get() == Some(entity);
        let active = matches!(action, MenuAction::Page(page) if *page == menu.page);
        let color = match *interaction {
            Interaction::Pressed => Color::srgb(0.32, 0.42, 0.53),
            Interaction::Hovered => Color::srgb(0.23, 0.28, 0.34),
            Interaction::None if active => Color::srgb(0.25, 0.34, 0.44),
            Interaction::None => Color::srgb(0.12, 0.15, 0.19),
        };
        background.set_if_neq(BackgroundColor(color));
        border.set_if_neq(BorderColor::all(if focused {
            Color::srgb(0.7, 0.8, 0.9)
        } else {
            Color::srgb(0.3, 0.35, 0.4)
        }));
        // Interactions remain Pressed while the mouse is held; only the transition activates.
        if (interaction.is_changed() && *interaction == Interaction::Pressed)
            || (focused && keyboard_activation)
        {
            focus.set(entity, FocusCause::Pressed);
            actions.push(*action);
        }
    }
    for action in actions {
        match action {
            MenuAction::Open => {
                menu.show(&mut capture, &mut time, &mut motion);
            }
            MenuAction::Page(page) => {
                menu.page = page;
                for mut position in &mut pages {
                    position.y = 0.0;
                }
            }
            MenuAction::Adjust(index, direction) => {
                SETTING_SPECS[index].adjust(&mut settings, direction);
                file.status = "Changed for this session. Choose Save to keep settings.".into();
            }
            MenuAction::CollisionDebug => {
                if let Some(debug) = debug.as_deref_mut() {
                    debug.enabled = !debug.enabled;
                } else {
                    file.status = "Collision outlines are unavailable in this session.".into();
                }
            }
            MenuAction::Save => file.save(&settings),
            MenuAction::Defaults => {
                *settings = RenderingSettings::default();
                file.status =
                    "Defaults restored for this session. Choose Save to keep them.".into();
            }
            MenuAction::Resume => {
                menu.close(&mut capture, &mut time);
                focus.clear();
            }
        }
    }
}

fn text_bundle(text: impl Into<String>, size: f32) -> impl Bundle {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor(Color::srgb(0.9, 0.92, 0.95)),
    )
}

fn button(parent: &mut ChildSpawnerCommands, label: &str, action: MenuAction, width: f32) {
    parent
        .spawn((
            Button,
            action,
            TabIndex(0),
            Node {
                min_width: Val::Px(width),
                min_height: Val::Px(32.0),
                flex_shrink: 0.0,
                padding: UiRect::axes(Val::Px(10.0), Val::Px(5.0)),
                border: UiRect::all(Val::Px(1.0)),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            BackgroundColor(Color::srgb(0.12, 0.15, 0.19)),
            BorderColor::all(Color::srgb(0.3, 0.35, 0.4)),
        ))
        .with_children(|parent| {
            parent.spawn(text_bundle(label, 16.0));
        });
}

fn spawn_settings_ui(mut commands: Commands, settings: Res<RenderingSettings>) {
    commands
        .spawn((
            MenuLauncher,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(12.0),
                right: Val::Px(12.0),
                ..default()
            },
            GlobalZIndex(900),
        ))
        .with_children(|root| {
            button(
                root,
                "Settings / Controls (Escape)",
                MenuAction::Open,
                180.0,
            );
        });
    commands.spawn((
        Name::new("Settings menu"), MenuRoot, TabGroup::modal(),
        Node {
            display: Display::None, position_type: PositionType::Absolute,
            width: Val::Percent(100.0), height: Val::Percent(100.0),
            justify_content: JustifyContent::Center, align_items: AlignItems::Center, ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.65)), GlobalZIndex(2000),
    )).observe(handle_tab_navigation).with_children(|root| {
        root.spawn((
            Node {
                width: Val::Percent(92.0), max_width: Val::Px(1050.0), height: Val::Percent(92.0),
                padding: UiRect::all(Val::Px(16.0)), flex_direction: FlexDirection::Column,
                row_gap: Val::Px(10.0), border: UiRect::all(Val::Px(1.0)), ..default()
            },
            BackgroundColor(Color::srgb(0.055, 0.065, 0.08)), BorderColor::all(Color::srgb(0.3, 0.35, 0.4)),
        )).with_children(|panel| {
            panel.spawn(text_bundle("Settings", 26.0));
            panel.spawn(text_bundle("Escape: resume    Tab: focus    Enter / Space: activate    Wheel / Page Up / Page Down: scroll", 14.0));
            panel.spawn(Node { column_gap: Val::Px(8.0), flex_shrink: 0.0, ..default() }).with_children(|tabs| {
                button(tabs, "Graphics", MenuAction::Page(SettingsPage::Graphics), 130.0);
                button(tabs, "Streaming", MenuAction::Page(SettingsPage::Streaming), 130.0);
                button(tabs, "Controls", MenuAction::Page(SettingsPage::Controls), 130.0);
            });
            for page in [SettingsPage::Graphics, SettingsPage::Streaming, SettingsPage::Controls] {
                panel.spawn((
                    PageRoot(page), ScrollPosition::default(),
                    Node {
                        display: if page == SettingsPage::Graphics { Display::Flex } else { Display::None },
                        flex_direction: FlexDirection::Column, flex_grow: 1.0, flex_basis: Val::Px(0.0),
                        min_height: Val::Px(0.0), overflow: Overflow::scroll_y(), row_gap: Val::Px(6.0),
                        padding: UiRect::right(Val::Px(10.0)), ..default()
                    },
                )).with_children(|body| {
                    if page == SettingsPage::Controls { spawn_controls(body); return; }
                    for (index, spec) in SETTING_SPECS.iter().enumerate().filter(|(_, spec)| spec.page == page) {
                        body.spawn(Node {
                            min_height: Val::Px(38.0), flex_shrink: 0.0, align_items: AlignItems::Center,
                            column_gap: Val::Px(8.0), ..default()
                        }).with_children(|row| {
                            row.spawn((text_bundle(spec.label, 16.0), Node { flex_grow: 1.0, flex_basis: Val::Px(0.0), ..default() }));
                            if !matches!(spec.kind, SettingKind::Toggle) { button(row, "-", MenuAction::Adjust(index, -1), 32.0); }
                            row.spawn((SettingValue(index), text_bundle(spec.value_label(&settings), 16.0), TextLayout::justify(Justify::Center), Node {
                                width: Val::Px(if matches!(spec.kind, SettingKind::Choice(_)) { 215.0 } else { 110.0 }),
                                justify_content: JustifyContent::Center, ..default()
                            }));
                            button(row, if matches!(spec.kind, SettingKind::Toggle) { "Toggle" } else { "+" }, MenuAction::Adjust(index, 1), 70.0);
                        });
                    }
                    if page == SettingsPage::Graphics {
                        button(body, "Toggle collision outlines (session only)", MenuAction::CollisionDebug, 250.0);
                        body.spawn((CollisionText, text_bundle("", 14.0)));
                        body.spawn(text_bundle("Room culling: not available yet", 16.0));
                        body.spawn(text_bundle("HDR composition: on. Material colors, texture tiling, and per-water factors come from converted assets.", 14.0));
                        body.spawn((RendererText, text_bundle("", 14.0)));
                        body.spawn(text_bundle("Occlusion culling needs depth prepass and indirect drawing; enabling it enables both.", 14.0));
                    } else {
                        body.spawn(text_bundle("Distance changes update streaming around the current camera. IO threads require Save and a restart.", 14.0));
                    }
                });
            }
            panel.spawn((StatusText, text_bundle("", 14.0), Node { flex_shrink: 0.0, ..default() }));
            panel.spawn(Node { column_gap: Val::Px(8.0), flex_shrink: 0.0, ..default() }).with_children(|footer| {
                button(footer, "Save", MenuAction::Save, 110.0);
                button(footer, "Reset defaults", MenuAction::Defaults, 170.0);
                button(footer, "Resume", MenuAction::Resume, 110.0);
            });
        });
    });
}

pub const CONTROL_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Menus and cursor",
        &[
            (
                "Escape",
                "Open / close settings and controls; close developer console",
            ),
            (
                "F3",
                "Release cursor while playing; left click captures it again",
            ),
            (
                "Left click",
                "Capture cursor while playing; activate menu buttons",
            ),
            ("Backtick", "Open / close developer console"),
            (
                "Tab / Shift+Tab",
                "Next / previous menu button; Tab completes console commands",
            ),
            (
                "Enter / Space",
                "Activate focused menu button; Enter submits console command",
            ),
            (
                "Wheel / Page Up / Page Down",
                "Scroll menu; Home / End jump to top / bottom",
            ),
        ],
    ),
    (
        "Movement (cursor captured)",
        &[
            ("Mouse", "Look around"),
            ("W / A / S / D", "Move relative to view"),
            (
                "V",
                "Toggle NOCLIP / WALK; walking requires a clear capsule placement",
            ),
            ("Space", "Fly up in NOCLIP; jump in WALK"),
            ("Ctrl", "Fly down in NOCLIP"),
            ("Shift", "Fast flight in NOCLIP; slow walking in WALK"),
            ("Alt", "Toggle sprint in WALK; stops when movement stops"),
        ],
    ),
    (
        "Debug tools",
        &[
            (
                "Graphics tab",
                "Toggle collision outlines in the streamed world",
            ),
            ("T", "Spawn a debug tankard on terrain (cursor captured)"),
            ("E", "Grab / drop a debug tankard (cursor captured)"),
            (
                "Console: Help",
                "List available commands; Help <text> filters them",
            ),
            ("Console: tcl / tcg", "Toggle NOCLIP / collision outlines"),
        ],
    ),
];

fn spawn_controls(parent: &mut ChildSpawnerCommands) {
    for (heading, controls) in CONTROL_GROUPS {
        parent.spawn((
            text_bundle(*heading, 21.0),
            Node {
                margin: UiRect::top(Val::Px(10.0)),
                flex_shrink: 0.0,
                ..default()
            },
        ));
        for (key, description) in *controls {
            parent
                .spawn(Node {
                    display: Display::Grid,
                    grid_template_columns: vec![GridTrack::px(235.0), GridTrack::flex(1.0)],
                    column_gap: Val::Px(12.0),
                    min_height: Val::Px(30.0),
                    flex_shrink: 0.0,
                    ..default()
                })
                .with_children(|row| {
                    row.spawn((text_bundle(*key, 16.0), Node::default()));
                    row.spawn((text_bundle(*description, 16.0), Node::default()));
                });
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn refresh_menu(
    menu: Res<SettingsMenuState>,
    settings: Res<RenderingSettings>,
    file: Res<SettingsFile>,
    adapter: Option<Res<RenderAdapterInfo>>,
    mut roots: Query<&mut Node, (With<MenuRoot>, Without<PageRoot>)>,
    mut pages: Query<(&PageRoot, &mut Node), Without<MenuRoot>>,
    mut launchers: Query<&mut Node, (With<MenuLauncher>, Without<MenuRoot>, Without<PageRoot>)>,
    mut text: Query<(
        &mut Text,
        Option<&SettingValue>,
        Has<StatusText>,
        Has<RendererText>,
        Has<CollisionText>,
    )>,
    collision: Option<Res<bevy_rapier3d::prelude::DebugRenderContext>>,
    console: Option<Res<ConsoleState>>,
    mut buttons: Query<(Entity, &MenuAction, &mut TabIndex)>,
    mut focus: ResMut<InputFocus>,
) {
    let console_open = console.as_ref().is_some_and(|c| c.open || c.closing);
    let mut focus_available = false;
    let mut initial_focus = None;
    // Bevy's tab traversal uses TabIndex, including on nodes hidden by Display::None.
    // Exclude the other pages and seed the modal group's focus when opening the menu.
    for (entity, action, mut index) in &mut buttons {
        let available = action.available(&menu, console_open);
        index.set_if_neq(TabIndex(if available { 0 } else { -1 }));
        focus_available |= available && focus.get() == Some(entity);
        if available && matches!(action, MenuAction::Page(page) if *page == menu.page) {
            initial_focus = Some(entity);
        }
    }
    if !focus_available {
        if let Some(entity) = initial_focus {
            focus.set(entity, FocusCause::Navigated);
        } else if focus.get().is_some() {
            focus.clear();
        }
    }
    for mut node in &mut roots {
        let display = if menu.open {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
    }
    for (page, mut node) in &mut pages {
        let display = if page.0 == menu.page {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
    }
    for mut node in &mut launchers {
        let display = if menu.open || console_open {
            Display::None
        } else {
            Display::Flex
        };
        if node.display != display {
            node.display = display;
        }
    }
    if !menu.open {
        return;
    }
    for (mut text, value, status, renderer, collision_text) in &mut text {
        let value = if let Some(value) = value {
            Some(SETTING_SPECS[value.0].value_label(&settings))
        } else if status {
            Some(file.status.clone())
        } else if renderer {
            Some(adapter.as_deref().map_or_else(
            || "Renderer information unavailable".into(),
            |adapter| format!("Renderer: {:?} / {}. Backend and GPU capabilities are selected at startup.", adapter.backend, adapter.name),
        ))
        } else if collision_text {
            Some(format!(
                "Collision outlines: {}",
                collision
                    .as_deref()
                    .map_or("unavailable", |debug| if debug.enabled {
                        "On"
                    } else {
                        "Off"
                    })
            ))
        } else {
            None
        };
        if let Some(value) = value
            && text.0 != value
        {
            text.0 = value;
        }
    }
}

fn scroll_focused_button(
    menu: Res<SettingsMenuState>,
    focus: Res<InputFocus>,
    nodes: Query<(&ComputedNode, &UiGlobalTransform)>,
    parents: Query<&ChildOf>,
    mut pages: Query<(
        &PageRoot,
        &ComputedNode,
        &UiGlobalTransform,
        &mut ScrollPosition,
    )>,
    mut previous: Local<Option<Entity>>,
) {
    if !menu.open || focus.get() == *previous {
        return;
    }
    *previous = focus.get();
    let Some(entity) = focus.get() else { return };
    let Ok((button, transform)) = nodes.get(entity) else {
        return;
    };
    for ancestor in parents.iter_ancestors(entity) {
        let Ok((page, viewport, viewport_transform, mut scroll)) = pages.get_mut(ancestor) else {
            continue;
        };
        if page.0 != menu.page {
            return;
        }
        let center = transform.translation.y - viewport_transform.translation.y;
        let top = center - button.size().y * 0.5 + viewport.size().y * 0.5;
        let bottom = top + button.size().y;
        let delta = if top < 0.0 {
            top
        } else if bottom > viewport.size().y {
            bottom - viewport.size().y
        } else {
            0.0
        };
        let scale = viewport.inverse_scale_factor();
        let maximum = ((viewport.content_size().y - viewport.size().y) * scale).max(0.0);
        scroll.y = (scroll.y + delta * scale).clamp(0.0, maximum);
        return;
    }
}

fn scroll_menu(
    menu: Res<SettingsMenuState>,
    keys: Res<ButtonInput<KeyCode>>,
    mut wheel: MessageReader<MouseWheel>,
    mut pages: Query<(&PageRoot, &ComputedNode, &mut ScrollPosition)>,
) {
    let mut delta = 0.0;
    for event in wheel.read() {
        delta -= event.y
            * if event.unit == MouseScrollUnit::Line {
                32.0
            } else {
                1.0
            };
    }
    if !menu.open {
        return;
    }
    if delta == 0.0
        && !keys.any_just_pressed([
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Home,
            KeyCode::End,
        ])
    {
        return;
    }
    for (page, computed, mut position) in &mut pages {
        if page.0 != menu.page {
            continue;
        }
        let height = computed.size().y * computed.inverse_scale_factor();
        let max = ((computed.content_size().y - computed.size().y)
            * computed.inverse_scale_factor())
        .max(0.0);
        if keys.just_pressed(KeyCode::PageDown) {
            delta += height * 0.8;
        }
        if keys.just_pressed(KeyCode::PageUp) {
            delta -= height * 0.8;
        }
        position.y = (position.y + delta).clamp(0.0, max);
        if keys.just_pressed(KeyCode::Home) {
            position.y = 0.0;
        }
        if keys.just_pressed(KeyCode::End) {
            position.y = max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::EngineConfig,
        console::{ConsolePlugin, console_closed},
        physics::{SprintLatch, WalkIntent},
    };
    use bevy::{
        ecs::system::RunSystemOnce,
        input::{
            ButtonState,
            keyboard::{Key, KeyboardInput},
        },
    };

    fn menu_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::input::InputPlugin, ConsolePlugin))
            .insert_resource(EngineConfig::default())
            .add_plugins(SettingsPlugin);
        app.finish();
        app.update();
        app
    }

    fn key(app: &mut App, code: KeyCode, state: ButtonState) {
        app.world_mut().write_message(KeyboardInput {
            key_code: code,
            logical_key: Key::Character("test".into()),
            state,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
    }

    #[test]
    fn opening_menu_clears_motion_and_closing_restores_capture_after_one_frame() {
        let mut app = menu_app();
        app.insert_resource(CursorCapture::Captured)
            .insert_resource(WalkIntent {
                jump_pressed: true,
                ..default()
            })
            .insert_resource(SprintLatch(true));
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().open);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        assert!(!app.world().resource::<WalkIntent>().jump_pressed);
        assert!(!app.world().resource::<SprintLatch>().0);
        assert!(!app.world_mut().run_system_once(console_closed).unwrap());
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().closing);
        assert!(!app.world_mut().run_system_once(console_closed).unwrap());
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
        assert!(app.world_mut().run_system_once(console_closed).unwrap());
        assert!(!app.world().resource::<Time<Virtual>>().is_paused());
    }

    #[test]
    fn menu_preserves_an_existing_pause_and_does_not_open_over_console() {
        let mut app = menu_app();
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(!app.world().resource::<SettingsMenuState>().open);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Backquote, ButtonState::Pressed);
        assert!(app.world().resource::<ConsoleState>().open);
        key(&mut app, KeyCode::Backquote, ButtonState::Released);
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        assert!(!app.world().resource::<SettingsMenuState>().open);
        assert!(app.world().resource::<ConsoleState>().open);
    }

    #[test]
    fn console_toggle_is_ignored_while_menu_owns_input() {
        let mut app = menu_app();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        key(&mut app, KeyCode::Backquote, ButtonState::Pressed);
        assert!(app.world().resource::<SettingsMenuState>().open);
        assert!(!app.world().resource::<ConsoleState>().open);
    }

    #[test]
    fn button_press_changes_setting_once_and_keyboard_can_activate_a_focused_button() {
        let mut app = menu_app();
        key(&mut app, KeyCode::Escape, ButtonState::Pressed);
        key(&mut app, KeyCode::Escape, ButtonState::Released);
        let button = app
            .world_mut()
            .query::<(Entity, &MenuAction)>()
            .iter(app.world())
            .find_map(|(entity, action)| {
                matches!(action, MenuAction::Adjust(0, 1)).then_some(entity)
            })
            .unwrap();
        app.world_mut()
            .entity_mut(button)
            .insert(Interaction::Pressed);
        app.update();
        assert!(app.world().resource::<RenderingSettings>().point_lights);
        app.update();
        assert!(app.world().resource::<RenderingSettings>().point_lights);
        app.world_mut().entity_mut(button).insert(Interaction::None);
        app.update();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(button, FocusCause::Navigated);
        key(&mut app, KeyCode::Space, ButtonState::Pressed);
        assert!(!app.world().resource::<RenderingSettings>().point_lights);
    }
}
