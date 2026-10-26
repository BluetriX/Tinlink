//! Who is online: users with at least one open gateway connection.

use std::collections::HashMap;
use std::sync::Mutex;

use uuid::Uuid;

#[derive(Default)]
pub struct Presence {
    connections: Mutex<HashMap<Uuid, usize>>,
}

impl Presence {
    /// Counts a new connection; returns true if the user just came online.
    pub fn connect(&self, user_id: Uuid) -> bool {
        let mut map = self.connections.lock().unwrap();
        let count = map.entry(user_id).or_default();
        *count += 1;
        *count == 1
    }

    /// Counts a closed connection; returns true if the user just went offline.
    pub fn disconnect(&self, user_id: Uuid) -> bool {
        let mut map = self.connections.lock().unwrap();
        match map.get_mut(&user_id) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => {
                map.remove(&user_id);
                true
            }
            None => false,
        }
    }

    pub fn is_online(&self, user_id: Uuid) -> bool {
        self.connections.lock().unwrap().contains_key(&user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_while_any_connection_is_open() {
        let p = Presence::default();
        let u = Uuid::now_v7();
        assert!(p.connect(u));
        assert!(!p.connect(u));
        assert!(!p.disconnect(u));
        assert!(p.is_online(u));
        assert!(p.disconnect(u));
        assert!(!p.is_online(u));
        assert!(!p.disconnect(u));
    }
}
