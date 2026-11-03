//! From touch positions to screen pixels: an affine map fitted to touches at known points.
//!
//! A touch controller reports positions in its own range, which may be swapped, inverted or
//! offset from the screen under it. [`Calibration::fit`] takes the positions reported for
//! targets drawn at known pixels and finds, by least squares, the map
//!
//! ```text
//! screen x = a * raw x + b * raw y + c
//! screen y = d * raw x + e * raw y + f
//! ```
//!
//! which covers scale, offset, swap, inversion and a small rotation. With more than three
//! targets, [`Calibration::residual`] shows how far each one lands from where it was drawn.

/// A position, in a controller's range or in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    /// Across.
    pub x: i32,
    /// Down.
    pub y: i32,
}

impl Point {
    /// A point at `x`, `y`.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// One target: the position the controller reported, and the pixel it was drawn at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// What the controller reported.
    pub raw: Point,
    /// Where the target was drawn.
    pub screen: Point,
}

/// The map from a controller's positions to pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    /// `a`, `b`, `c` of the screen X.
    pub x: [f32; 3],
    /// `d`, `e`, `f` of the screen Y.
    pub y: [f32; 3],
}

fn round(value: f32) -> i32 {
    if value >= 0.0 { (value + 0.5) as i32 } else { (value - 0.5) as i32 }
}

fn det3(m: [[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Solves `m * v = rhs` by Cramer's rule, given `det` of `m`, not zero.
fn solve3(m: [[f64; 3]; 3], det: f64, rhs: [f64; 3]) -> [f32; 3] {
    let mut v = [0.0; 3];
    for (column, value) in v.iter_mut().enumerate() {
        let mut replaced = m;
        for row in 0..3 {
            replaced[row][column] = rhs[row];
        }
        *value = (det3(replaced) / det) as f32;
    }
    v
}

impl Calibration {
    /// Scales `0..=x_range` and `0..=y_range` onto `width` and `height` pixels, unswapped:
    /// what a controller set up for this screen reports.
    pub fn scale(x_range: u16, y_range: u16, width: usize, height: usize) -> Self {
        let x = (width.max(1) - 1) as f32 / f32::from(x_range.max(1));
        let y = (height.max(1) - 1) as f32 / f32::from(y_range.max(1));
        Self { x: [x, 0.0, 0.0], y: [0.0, y, 0.0] }
    }

    /// The least-squares map through `samples`. `None` with fewer than three, or when they
    /// lie on one line, where the map is not determined.
    pub fn fit(samples: &[Sample]) -> Option<Self> {
        if samples.len() < 3 {
            return None;
        }
        // The normal equations, raw positions taken from their mean so that the sums stay
        // small and the solution precise.
        let count = samples.len() as f64;
        let mean_x = samples.iter().map(|s| f64::from(s.raw.x)).sum::<f64>() / count;
        let mean_y = samples.iter().map(|s| f64::from(s.raw.y)).sum::<f64>() / count;
        let mut m = [[0.0f64; 3]; 3];
        let (mut rhs_x, mut rhs_y) = ([0.0f64; 3], [0.0f64; 3]);
        for sample in samples {
            let row = [f64::from(sample.raw.x) - mean_x, f64::from(sample.raw.y) - mean_y, 1.0];
            for i in 0..3 {
                for j in 0..3 {
                    m[i][j] += row[i] * row[j];
                }
                rhs_x[i] += row[i] * f64::from(sample.screen.x);
                rhs_y[i] += row[i] * f64::from(sample.screen.y);
            }
        }
        let det = det3(m);
        // Relative to the spread of the points: collinear ones leave only rounding.
        let scale = m[0][0] * m[1][1] * m[2][2];
        if scale <= 0.0 || (if det < 0.0 { -det } else { det }) < scale * 1e-6 {
            return None;
        }
        let [a, b, c] = solve3(m, det, rhs_x);
        let [d, e, f] = solve3(m, det, rhs_y);
        // Back from centered positions: c' = c - a * mean_x - b * mean_y.
        let (mean_x, mean_y) = (mean_x as f32, mean_y as f32);
        Some(Self { x: [a, b, c - a * mean_x - b * mean_y], y: [d, e, f - d * mean_x - e * mean_y] })
    }

    /// The pixel `raw` lands on, rounded, and not limited to the screen.
    pub fn map(&self, raw: Point) -> Point {
        let (x, y) = (raw.x as f32, raw.y as f32);
        Point::new(round(self.x[0] * x + self.x[1] * y + self.x[2]), round(self.y[0] * x + self.y[1] * y + self.y[2]))
    }

    /// How far `sample` lands from its target, in pixels across and down.
    pub fn residual(&self, sample: &Sample) -> Point {
        let mapped = self.map(sample.raw);
        Point::new(mapped.x - sample.screen.x, mapped.y - sample.screen.y)
    }
}
