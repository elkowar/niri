//! Helpers for keeping track of the event stream state.

use std::collections::HashMap;

use crate::{Event, KeyboardLayouts, Workspace};

/// Part of the state communicated via the event stream.
pub trait EventStreamStatePart {
    /// Returns a sequence of events that replicates this state from default initialization.
    fn replicate(&self) -> Vec<Event>;

    /// Applies the event to this state.
    ///
    /// Returns `None` after applying the event, and `Some(event)` if the event is ignored by this
    /// part of the state.
    fn apply(&mut self, event: Event) -> Option<Event>;
}

/// The full state communicated over the event stream.
#[derive(Debug, Default)]
pub struct EventStreamState {
    /// State of workspaces.
    pub workspaces: WorkspacesState,

    /// State of the keyboard layouts.
    pub keyboard_layouts: KeyboardLayoutsState,
}

/// The workspaces state communicated over the event stream.
#[derive(Debug, Default)]
pub struct WorkspacesState {
    /// Map from a workspace id to the workspace.
    pub workspaces: HashMap<u64, Workspace>,
}

/// The keyboard layout state communicated over the event stream.
#[derive(Debug, Default)]
pub struct KeyboardLayoutsState {
    /// Configured keyboard layouts.
    pub keyboard_layouts: Option<KeyboardLayouts>,
}

impl EventStreamStatePart for EventStreamState {
    fn replicate(&self) -> Vec<Event> {
        let mut events = Vec::new();
        events.extend(self.workspaces.replicate());
        events.extend(self.keyboard_layouts.replicate());
        events
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        let event = self.workspaces.apply(event)?;
        let event = self.keyboard_layouts.apply(event)?;
        Some(event)
    }
}

impl EventStreamStatePart for WorkspacesState {
    fn replicate(&self) -> Vec<Event> {
        let workspaces = self.workspaces.values().cloned().collect();
        vec![Event::WorkspacesChanged { workspaces }]
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::WorkspacesChanged { workspaces } => {
                self.workspaces = workspaces.into_iter().map(|ws| (ws.id, ws)).collect();
            }
            Event::WorkspaceFocused { id } => {
                let ws = self.workspaces.get(&id);
                let ws = ws.expect("focused workspace was missing from the map");
                let output = ws.output.clone();

                for ws in self.workspaces.values_mut() {
                    if ws.output == output {
                        ws.is_active = ws.id == id;
                    }

                    ws.is_focused = ws.id == id;
                }
            }
            Event::WorkspaceActivated { id } => {
                let ws = self.workspaces.get(&id);
                let ws = ws.expect("activated workspace was missing from the map");
                let output = ws.output.clone();

                for ws in self.workspaces.values_mut() {
                    if ws.output == output {
                        ws.is_active = ws.id == id;
                    }
                }
            }
            event => return Some(event),
        }
        None
    }
}

impl EventStreamStatePart for KeyboardLayoutsState {
    fn replicate(&self) -> Vec<Event> {
        if let Some(keyboard_layouts) = self.keyboard_layouts.clone() {
            vec![Event::KeyboardLayoutsChanged { keyboard_layouts }]
        } else {
            vec![]
        }
    }

    fn apply(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::KeyboardLayoutsChanged { keyboard_layouts } => {
                self.keyboard_layouts = Some(keyboard_layouts);
            }
            Event::KeyboardLayoutSwitched { idx } => {
                let kb = self.keyboard_layouts.as_mut();
                let kb = kb.expect("keyboard layouts must be set before a layout can be switched");
                kb.current_idx = idx;
            }
            event => return Some(event),
        }
        None
    }
}
