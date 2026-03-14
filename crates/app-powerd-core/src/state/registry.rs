use std::collections::HashMap;

use super::app_entry::{AppEntry, AppId};

/// Registry of all tracked applications.
pub struct AppRegistry {
    apps: HashMap<AppId, AppEntry>,
    /// Map window_id → AppId for quick lookup.
    window_map: HashMap<u64, AppId>,
}

impl AppRegistry {
    pub fn new() -> Self {
        Self {
            apps: HashMap::new(),
            window_map: HashMap::new(),
        }
    }

    /// Get app by AppId.
    pub fn get(&self, app_id: &AppId) -> Option<&AppEntry> {
        self.apps.get(app_id)
    }

    /// Get mutable app by AppId.
    pub fn get_mut(&mut self, app_id: &AppId) -> Option<&mut AppEntry> {
        self.apps.get_mut(app_id)
    }

    /// Insert or update an app entry. Returns the AppId.
    pub fn insert(&mut self, entry: AppEntry) -> AppId {
        let app_id = entry.app_id.clone();
        for &wid in &entry.window_ids {
            self.window_map.insert(wid, app_id.clone());
        }
        self.apps.insert(app_id.clone(), entry);
        app_id
    }

    /// Remove a window. If the app has no more windows, remove the app and return it.
    pub fn remove_window(&mut self, window_id: u64) -> Option<AppEntry> {
        let app_id = self.window_map.remove(&window_id)?;
        let entry = self.apps.get_mut(&app_id)?;

        if entry.remove_window(window_id) {
            // No more windows — remove the app entirely
            self.apps.remove(&app_id)
        } else {
            None
        }
    }

    /// Iterate over all apps.
    pub fn iter(&self) -> impl Iterator<Item = (&AppId, &AppEntry)> {
        self.apps.iter()
    }

    /// Iterate mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&AppId, &mut AppEntry)> {
        self.apps.iter_mut()
    }

    /// Number of tracked apps.
    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }
}

impl Default for AppRegistry {
    fn default() -> Self {
        Self::new()
    }
}
