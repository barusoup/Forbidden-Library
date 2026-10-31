//! Shared motion for the chrome. Anything that should move in step reads from
//! the same spring, so the island and the header title cannot drift apart.

/// A critically damped spring: it eases to its target without overshoot, and
/// it keeps its velocity when the target moves mid-flight, so rapid clicks
/// bend the motion instead of restarting it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spring {
    pub x: f32,
    pub v: f32,
}

impl Spring {
    pub fn at(x: f32) -> Self {
        Self { x, v: 0.0 }
    }

    /// Advance by `dt` seconds. `omega` sets the speed: higher settles sooner.
    pub fn step(&mut self, target: f32, dt: f32, omega: f32) {
        // Small substeps keep the integration stable on slow frames.
        const SUBSTEPS: u32 = 4;
        let h = dt / SUBSTEPS as f32;
        for _ in 0..SUBSTEPS {
            let accel = omega * omega * (target - self.x) - 2.0 * omega * self.v;
            self.v += accel * h;
            self.x += self.v * h;
        }
        if (target - self.x).abs() < 1e-4 && self.v.abs() < 1e-3 {
            self.x = target;
            self.v = 0.0;
        }
    }

    pub fn settled(&self, target: f32) -> bool {
        self.x == target && self.v == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settles_without_overshoot() {
        let mut spring = Spring::at(0.0);
        let mut peak = 0.0_f32;
        for _ in 0..120 {
            spring.step(1.0, 1.0 / 60.0, 24.0);
            peak = peak.max(spring.x);
        }
        assert!(spring.settled(1.0), "spring did not settle: {spring:?}");
        assert!(peak <= 1.0 + 1e-4, "spring overshot to {peak}");
    }

    #[test]
    fn retargeting_keeps_velocity() {
        let mut spring = Spring::at(0.0);
        for _ in 0..6 {
            spring.step(1.0, 1.0 / 60.0, 24.0);
        }
        let moving = spring.v;
        assert!(moving > 0.0);
        // Reversing the target slows the spring and keeps it moving forward
        // for a frame, rather than snapping its velocity to zero.
        spring.step(0.0, 1.0 / 60.0, 24.0);
        assert!(spring.v < moving);
        assert!(spring.v.abs() > 0.0);
    }
}
