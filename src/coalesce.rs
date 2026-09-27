//! request coalescing: while one request for a cache key is on its way to the
//! origin, other requests for the key wait for it and then read the cache.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use tokio::sync::watch;

type InFlight<K> = Arc<Mutex<HashMap<K, watch::Receiver<()>>>>;

/// the keys with a request in flight, shared by every worker.
pub struct Flights<K> {
    in_flight: InFlight<K>,
}

impl<K> Default for Flights<K> {
    fn default() -> Self {
        Flights {
            in_flight: Arc::default(),
        }
    }
}

/// a request's part in the flight for its key.
pub enum Join<K: Hash + Eq> {
    /// the request goes to the origin, and others wait until the guard drops
    Lead(FlightGuard<K>),
    /// another request is in flight for the key
    Follow(Waiter),
    /// nothing is in flight and the request may not lead
    Alone,
}

impl<K: Hash + Eq + Clone> Flights<K> {
    /// joins the flight for `key`, or starts one when there is none and
    /// `may_lead` holds.
    pub fn join(&self, key: K, may_lead: bool) -> Join<K> {
        let mut in_flight = self.in_flight.lock();
        if let Some(done) = in_flight.get(&key) {
            return Join::Follow(Waiter(done.clone()));
        }
        if !may_lead {
            return Join::Alone;
        }
        let (sender, done) = watch::channel(());
        in_flight.insert(key.clone(), done);
        Join::Lead(FlightGuard {
            in_flight: self.in_flight.clone(),
            key,
            _sender: sender,
        })
    }
}

/// the leader's claim on a key. dropping it ends the flight and wakes the
/// followers.
pub struct FlightGuard<K: Hash + Eq> {
    in_flight: InFlight<K>,
    key: K,
    /// dropped after `drop` has removed the key, so a woken follower never
    /// finds the finished flight
    _sender: watch::Sender<()>,
}

impl<K: Hash + Eq> Drop for FlightGuard<K> {
    fn drop(&mut self) {
        self.in_flight.lock().remove(&self.key);
    }
}

/// a follower's view of a flight.
pub struct Waiter(watch::Receiver<()>);

impl Waiter {
    /// returns once the leader's guard has dropped.
    pub async fn wait(mut self) {
        // nothing is ever sent, so `changed` returns only when the sender
        // drops, or at once if it already has
        while self.0.changed().await.is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lead(flights: &Flights<&'static str>, key: &'static str) -> FlightGuard<&'static str> {
        match flights.join(key, true) {
            Join::Lead(guard) => guard,
            _ => panic!("{key} did not lead"),
        }
    }

    fn follow(flights: &Flights<&'static str>, key: &'static str) -> Waiter {
        match flights.join(key, true) {
            Join::Follow(waiter) => waiter,
            _ => panic!("{key} did not follow"),
        }
    }

    async fn released(waiter: Waiter) -> bool {
        tokio::time::timeout(Duration::from_millis(100), waiter.wait())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn followers_wait_until_the_guard_drops() {
        let flights = Flights::default();
        let guard = lead(&flights, "a");

        assert!(!released(follow(&flights, "a")).await);
        let waiter = follow(&flights, "a");
        drop(guard);
        assert!(released(waiter).await);
    }

    #[tokio::test]
    async fn a_follower_that_waits_after_the_guard_dropped_is_released() {
        let flights = Flights::default();
        let guard = lead(&flights, "a");
        let waiter = follow(&flights, "a");
        drop(guard);

        assert!(released(waiter).await);
    }

    #[test]
    fn the_next_request_leads_once_the_flight_ends() {
        let flights = Flights::default();
        drop(lead(&flights, "a"));

        lead(&flights, "a");
    }

    #[test]
    fn keys_have_separate_flights() {
        let flights = Flights::default();
        let _a = lead(&flights, "a");

        lead(&flights, "b");
    }

    #[test]
    fn a_request_that_may_not_lead_goes_alone() {
        let flights = Flights::default();

        assert!(matches!(flights.join("a", false), Join::Alone));
        let _guard = lead(&flights, "a");
        assert!(matches!(flights.join("a", false), Join::Follow(_)));
    }
}
