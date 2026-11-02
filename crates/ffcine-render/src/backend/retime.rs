//! Frame selection after decode, matching the ffmpeg CLI the prototype runs: `-ss` input seeking
//! (accurate seek trims frames before the start), `setpts=PTS-STARTPTS`, the `fps` filter
//! (`round=near`), `-frames:v N` and `tpad=stop_mode=clone`.

/// A rational time base or rate, `num/den`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Q {
    pub num: i64,
    pub den: i64,
}

impl Q {
    pub(crate) const fn new(num: i64, den: i64) -> Self {
        Self { num, den }
    }
}

/// `av_rescale_q_rnd(v, from, to, AV_ROUND_NEAR_INF)`: round half away from zero.
pub(crate) fn rescale_near(v: i64, from: Q, to: Q) -> i64 {
    let n = i128::from(v) * i128::from(from.num) * i128::from(to.den);
    let d = i128::from(from.den) * i128::from(to.num);
    let r = if (n < 0) == (d < 0) {
        (n.abs() + d.abs() / 2) / d.abs()
    } else {
        -((n.abs() + d.abs() / 2) / d.abs())
    };
    i64::try_from(r).unwrap_or(if r < 0 { i64::MIN } else { i64::MAX })
}

/// Picks output frames from decoded ones. Generic so the timing logic is testable without libav.
#[derive(Debug)]
pub(crate) struct Retimer<T> {
    tb: Q,
    /// Frames with `pts < trim` (stream time base) are dropped (accurate seek), except the last
    /// `lead` of them: they come out ahead of the first kept frame, as is (temporal filter
    /// history; [`Retimer::led`] counts them).
    trim: i64,
    lead: usize,
    held: std::collections::VecDeque<T>,
    led: usize,
    /// Output frame rate's time base (`1/fps`), or `None` to pass frames through.
    out_tb: Option<Q>,
    first: Option<i64>,
    pending: Option<T>,
    next: i64,
}

impl<T: Clone> Retimer<T> {
    /// `tb`: the stream time base. `trim`: first pts to keep. `fps`: output rate.
    pub(crate) fn new(tb: Q, trim: i64, fps: Option<Q>) -> Self {
        Self {
            tb,
            trim,
            lead: 0,
            held: std::collections::VecDeque::new(),
            led: 0,
            out_tb: fps.map(|f| Q::new(f.den, f.num)),
            first: None,
            pending: None,
            next: 0,
        }
    }

    /// Keeps up to `n` frames before the trim point.
    pub(crate) fn lead(mut self, n: usize) -> Self {
        self.lead = n;
        self
    }

    /// Frames output ahead of the first kept one (set once that frame is in).
    pub(crate) fn led(&self) -> usize {
        self.led
    }

    /// Feeds one decoded frame (presentation order); returns the output frames it completes.
    pub(crate) fn push(&mut self, pts: i64, frame: T, out: &mut Vec<T>) {
        if pts < self.trim {
            if self.lead > 0 {
                if self.held.len() == self.lead {
                    self.held.pop_front();
                }
                self.held.push_back(frame);
            }
            return;
        }
        if self.first.is_none() {
            self.led = self.held.len();
            out.extend(self.held.drain(..));
        }
        let first = *self.first.get_or_insert(pts);
        let Some(out_tb) = self.out_tb else {
            out.push(frame);
            return;
        };
        let t = rescale_near(pts - first, self.tb, out_tb);
        match self.pending.take() {
            None => self.next = t,
            Some(prev) => {
                // vf_fps: slot n shows the last frame whose rounded pts <= n.
                while self.next < t {
                    out.push(prev.clone());
                    self.next += 1;
                }
            }
        }
        self.pending = Some(frame);
    }

    /// End of input: the last frame is shown once more (at least).
    pub(crate) fn finish(&mut self, out: &mut Vec<T>) {
        if let Some(p) = self.pending.take() {
            out.push(p);
            self.next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(tb: Q, trim: i64, fps: Option<Q>, pts: &[i64]) -> Vec<i64> {
        let mut r = Retimer::new(tb, trim, fps);
        let mut out = Vec::new();
        for &p in pts {
            r.push(p, p, &mut out);
        }
        r.finish(&mut out);
        out
    }

    #[test]
    fn rounding() {
        assert_eq!(rescale_near(1, Q::new(1, 2), Q::new(1, 1)), 1);
        assert_eq!(rescale_near(-1, Q::new(1, 2), Q::new(1, 1)), -1);
        assert_eq!(rescale_near(6006, Q::new(1, 24000), Q::new(1001, 24000)), 6);
    }

    #[test]
    fn dji_same_rate_trims_at_seek_point() {
        // payday: 23.976 fps in 1/24000, -ss 0.25 -> first kept frame is #6 (pts 6006 >= 6000).
        let tb = Q::new(1, 24000);
        let pts: Vec<i64> = (0..12).map(|k| k * 1001).collect();
        let out = run(tb, 6000, Some(Q::new(24000, 1001)), &pts);
        assert_eq!(out, (6..12).map(|k| k * 1001).collect::<Vec<_>>());
    }

    #[test]
    fn r10_25_to_23976_drops_frames() {
        // 25 fps -> 23.976: about one frame in 25 is dropped, never duplicated.
        let tb = Q::new(1, 25000);
        let pts: Vec<i64> = (0..100).map(|k| k * 1000).collect();
        let out = run(tb, 0, Some(Q::new(24000, 1001)), &pts);
        assert!(out.len() < pts.len());
        assert!(out.windows(2).all(|w| w[0] < w[1]));
        // slot n = last frame k with round(k * 0.95904) <= n
        let pick = |n: i64| {
            (0..100)
                .rev()
                .find(|k| rescale_near(k * 1000, tb, Q::new(1001, 24000)) <= n)
                .unwrap()
                * 1000
        };
        for (n, &p) in out.iter().enumerate().take(90) {
            assert_eq!(p, pick(i64::try_from(n).unwrap()), "slot {n}");
        }
    }

    #[test]
    fn upsampling_duplicates() {
        let out = run(Q::new(1, 10), 0, Some(Q::new(20, 1)), &[0, 1, 2]);
        assert_eq!(out, vec![0, 0, 1, 1, 2]);
    }

    #[test]
    fn lead_frames_come_first_and_leave_the_rest_alone() {
        let tb = Q::new(1, 24000);
        let fps = Some(Q::new(24000, 1001));
        let pts: Vec<i64> = (0..12).map(|k| k * 1001).collect();
        let mut r = Retimer::new(tb, 6000, fps).lead(2);
        let mut out = Vec::new();
        for &p in &pts {
            r.push(p, p, &mut out);
        }
        r.finish(&mut out);
        assert_eq!(r.led(), 2);
        assert_eq!(out[..2], [4004, 5005]);
        assert_eq!(out[2..], run(tb, 6000, fps, &pts)[..]);
        // Near the start of the source there are fewer.
        let mut r = Retimer::new(tb, 1001, fps).lead(3);
        let mut out = Vec::new();
        for &p in &pts[..4] {
            r.push(p, p, &mut out);
        }
        assert_eq!(r.led(), 1);
        assert_eq!(out[0], 0);
    }

    #[test]
    fn passthrough() {
        assert_eq!(run(Q::new(1, 25), 2, None, &[0, 1, 2, 3]), vec![2, 3]);
    }
}
