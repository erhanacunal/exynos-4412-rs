//! The little 3D arithmetic drawing needs, without `std`: sine and cosine, square roots, 4 x 4
//! matrices and 3-vectors.

use core::f32::consts::TAU;
use core::ops::Mul;

/// Sine and cosine of `angle`, in radians. Taylor series to the 13th power after bringing the
/// angle into -pi..pi: within 3e-5 there, ample for pixels.
pub fn sin_cos(angle: f32) -> (f32, f32) {
    let turns = angle / TAU;
    let whole = if turns >= 0.0 { (turns + 0.5) as i32 } else { (turns - 0.5) as i32 };
    let x = (turns - whole as f32) * TAU;
    let x2 = x * x;
    let (mut sin, mut cos) = (x, 1.0);
    let (mut sin_term, mut cos_term) = (x, 1.0);
    for n in 1..=6 {
        let n = n as f32;
        sin_term *= -x2 / ((2.0 * n) * (2.0 * n + 1.0));
        cos_term *= -x2 / ((2.0 * n - 1.0) * (2.0 * n));
        sin += sin_term;
        cos += cos_term;
    }
    (sin, cos)
}

/// The square root of `value`, by Newton's method; 0 for 0 and below.
pub fn sqrt(value: f32) -> f32 {
    if value <= 0.0 {
        return 0.0;
    }
    let mut root = if value > 1.0 { value } else { 1.0 };
    for _ in 0..30 {
        let next = 0.5 * (root + value / root);
        if next == root {
            break;
        }
        root = next;
    }
    root
}

/// A direction or a point: x, y, z.
pub type Vec3 = [f32; 3];

/// The dot product of `a` and `b`.
pub fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// `v` scaled to length 1; `v` itself if it is all zeros.
pub fn normalize(v: Vec3) -> Vec3 {
    let length = sqrt(dot(v, v));
    if length == 0.0 { v } else { [v[0] / length, v[1] / length, v[2] / length] }
}

/// A 4 x 4 matrix as GLSL takes one for a `mat4` uniform: column after column,
/// `m[column * 4 + row]`. `a * b` applies `b` first, then `a`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4(pub [f32; 16]);

impl Mat4 {
    /// Changes nothing.
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);

    /// Moves by `x`, `y`, `z`.
    pub const fn translation(x: f32, y: f32, z: f32) -> Self {
        let mut m = Self::IDENTITY.0;
        m[12] = x;
        m[13] = y;
        m[14] = z;
        Self(m)
    }

    /// Scales by `x`, `y`, `z`.
    pub const fn scale(x: f32, y: f32, z: f32) -> Self {
        let mut m = Self::IDENTITY.0;
        m[0] = x;
        m[5] = y;
        m[10] = z;
        Self(m)
    }

    /// Turns by `angle` radians about the x axis: y towards z.
    pub fn rotation_x(angle: f32) -> Self {
        let (sin, cos) = sin_cos(angle);
        let mut m = Self::IDENTITY.0;
        (m[5], m[6], m[9], m[10]) = (cos, sin, -sin, cos);
        Self(m)
    }

    /// Turns by `angle` radians about the y axis: z towards x.
    pub fn rotation_y(angle: f32) -> Self {
        let (sin, cos) = sin_cos(angle);
        let mut m = Self::IDENTITY.0;
        (m[0], m[2], m[8], m[10]) = (cos, -sin, sin, cos);
        Self(m)
    }

    /// Turns by `angle` radians about the z axis: x towards y.
    pub fn rotation_z(angle: f32) -> Self {
        let (sin, cos) = sin_cos(angle);
        let mut m = Self::IDENTITY.0;
        (m[0], m[1], m[4], m[5]) = (cos, sin, -sin, cos);
        Self(m)
    }

    /// A perspective, the camera at the origin looking down -z: `fov_y` radians from the frame's
    /// bottom to its top, `aspect` its width over its height, and what is between `near` and
    /// `far` in front of the camera drawn (`gluPerspective`).
    pub fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Self {
        let (sin, cos) = sin_cos(fov_y / 2.0);
        let focal = cos / sin;
        let mut m = [0.0; 16];
        m[0] = focal / aspect;
        m[5] = focal;
        m[10] = (far + near) / (near - far);
        m[11] = -1.0;
        m[14] = 2.0 * far * near / (near - far);
        Self(m)
    }

    /// The point `p` through the matrix, w taken as 1: x, y, z and w.
    pub fn transform(&self, p: Vec3) -> [f32; 4] {
        let m = &self.0;
        core::array::from_fn(|row| m[row] * p[0] + m[4 + row] * p[1] + m[8 + row] * p[2] + m[12 + row])
    }

    /// The direction `d` through the matrix's upper 3 x 3 part: turned, not moved.
    pub fn transform_direction(&self, d: Vec3) -> Vec3 {
        let m = &self.0;
        core::array::from_fn(|row| m[row] * d[0] + m[4 + row] * d[1] + m[8 + row] * d[2])
    }

    /// The direction `d` through the transpose of the upper 3 x 3 part: for a rotation, `d`
    /// turned back, as a light's direction into an object's own space.
    pub fn transform_direction_back(&self, d: Vec3) -> Vec3 {
        let m = &self.0;
        core::array::from_fn(|column| m[column * 4] * d[0] + m[column * 4 + 1] * d[1] + m[column * 4 + 2] * d[2])
    }
}

impl Mul for Mat4 {
    type Output = Self;

    fn mul(self, other: Self) -> Self {
        let (a, b) = (&self.0, &other.0);
        Self(core::array::from_fn(|index| {
            let (column, row) = (index / 4, index % 4);
            (0..4).map(|k| a[k * 4 + row] * b[column * 4 + k]).sum()
        }))
    }
}
