//! The geometric types of `src/backend/utils/adt/geo_ops.c`: `point`, `lseg`, `box`, `path`, `polygon`, `line` and `circle`. A value is the bytes of its binary form, so that the send function is a copy of the bytes.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::float::{float8_out, strtod};
use crate::number::is_space;
use crate::oid;

/// `EPSILON`: two numbers that differ by no more than this are equal in the fuzzy comparisons of `geo_decls.h`.
const EPSILON: f64 = 1.0e-6;

/// A geometric type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoKind {
    Point,
    Lseg,
    Box,
    Path,
    Polygon,
    Line,
    Circle,
}

impl GeoKind {
    /// The geometric type with this OID, or `None` for another type.
    #[must_use]
    pub fn from_oid(ty: u32) -> Option<GeoKind> {
        Some(match ty {
            oid::POINT => GeoKind::Point,
            oid::LSEG => GeoKind::Lseg,
            oid::BOX => GeoKind::Box,
            oid::PATH => GeoKind::Path,
            oid::POLYGON => GeoKind::Polygon,
            oid::LINE => GeoKind::Line,
            oid::CIRCLE => GeoKind::Circle,
            _ => return None,
        })
    }

    /// The name of the type in an error.
    fn name(self) -> &'static str {
        match self {
            GeoKind::Point => "point",
            GeoKind::Lseg => "lseg",
            GeoKind::Box => "box",
            GeoKind::Path => "path",
            GeoKind::Polygon => "polygon",
            GeoKind::Line => "line",
            GeoKind::Circle => "circle",
        }
    }
}

/// A point of a geometric value.
#[derive(Clone, Copy, Debug)]
struct Point {
    x: f64,
    y: f64,
}

/// `FPzero`.
fn fp_zero(a: f64) -> bool {
    a.abs() <= EPSILON
}

/// `FPeq`.
fn fp_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= EPSILON
}

/// `float8_lt`: NaN is above every other value.
fn float8_lt(a: f64, b: f64) -> bool {
    !a.is_nan() && (b.is_nan() || a < b)
}

/// `float8_eq`: NaN is equal to NaN.
fn float8_eq(a: f64, b: f64) -> bool {
    if a.is_nan() || b.is_nan() { a.is_nan() && b.is_nan() } else { a == b }
}

/// `point_eq_point`: the fuzzy equality, or the exact equality when a coordinate is NaN.
fn point_eq(a: Point, b: Point) -> bool {
    if a.x.is_nan() || a.y.is_nan() || b.x.is_nan() || b.y.is_nan() {
        return float8_eq(a.x, b.x) && float8_eq(a.y, b.y);
    }
    fp_eq(a.x, b.x) && fp_eq(a.y, b.y)
}

/// `22003`, `value out of range: overflow` or `value out of range: underflow`.
fn out_of_range(what: &str) -> TypeError {
    TypeError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("value out of range: {what}"))
}

/// `float8_mi`.
fn float8_mi(a: f64, b: f64) -> Result<f64, TypeError> {
    let r = a - b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(out_of_range("overflow"));
    }
    Ok(r)
}

/// `float8_mul`.
fn float8_mul(a: f64, b: f64) -> Result<f64, TypeError> {
    let r = a * b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(out_of_range("overflow"));
    }
    if r == 0.0 && a != 0.0 && b != 0.0 {
        return Err(out_of_range("underflow"));
    }
    Ok(r)
}

/// `float8_div`.
fn float8_div(a: f64, b: f64) -> Result<f64, TypeError> {
    if b == 0.0 && !a.is_nan() {
        return Err(TypeError::new(SqlState::DIVISION_BY_ZERO, "division by zero".to_string()));
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        return Err(out_of_range("overflow"));
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        return Err(out_of_range("underflow"));
    }
    Ok(r)
}

/// `point_sl`: the slope of the line through two points, which is infinity for a vertical line.
fn point_sl(a: Point, b: Point) -> Result<f64, TypeError> {
    if fp_eq(a.x, b.x) {
        return Ok(f64::INFINITY);
    }
    if fp_eq(a.y, b.y) {
        return Ok(0.0);
    }
    float8_div(float8_mi(a.y, b.y)?, float8_mi(a.x, b.x)?)
}

/// `line_construct`: the coefficients `A`, `B` and `C` of `Ax + By + C = 0` for the line through a point with a slope.
fn line_construct(pt: Point, m: f64) -> Result<[f64; 3], TypeError> {
    if m.is_infinite() {
        return Ok([-1.0, 0.0, pt.x]);
    }
    if m == 0.0 {
        return Ok([0.0, -1.0, pt.y]);
    }
    let c = float8_mi(pt.y, float8_mul(m, pt.x)?)?;
    // The subtraction can give -0, which PostgreSQL changes to 0.
    Ok([m, -1.0, if c == 0.0 { 0.0 } else { c }])
}

/// The reader of the text form of a geometric value, with the functions of `geo_ops.c` that read the parts. Each function takes the index of a byte and gives the index after the part.
struct Reader<'a> {
    text: &'a str,
    kind: GeoKind,
}

impl Reader<'_> {
    /// The byte at `i`, or 0 at the end, as a C string has.
    fn at(&self, i: usize) -> u8 {
        self.text.as_bytes().get(i).copied().unwrap_or(0)
    }

    fn skip_space(&self, mut i: usize) -> usize {
        while is_space(self.at(i)) {
            i += 1;
        }
        i
    }

    /// `22P02`, `invalid input syntax for type point: "..."`.
    fn syntax(&self) -> TypeError {
        TypeError::syntax(self.kind.name(), self.text)
    }

    /// An error if `i` is not the end of the text.
    fn end(&self, i: usize) -> Result<(), TypeError> {
        if self.at(i) == 0 { Ok(()) } else { Err(self.syntax()) }
    }

    /// An error if the byte at `i` is not `c`, else the index after it.
    fn expect(&self, i: usize, c: u8) -> Result<usize, TypeError> {
        if self.at(i) == c { Ok(i + 1) } else { Err(self.syntax()) }
    }

    /// `single_decode` and `float8in_internal`: a number with the white space around it.
    fn single(&self, i: usize) -> Result<(f64, usize), TypeError> {
        let i = self.skip_space(i);
        if self.at(i) == 0 {
            return Err(self.syntax());
        }
        let Some((value, len, range)) = strtod::<f64>(&self.text.as_bytes()[i..]) else {
            return Err(self.syntax());
        };
        if range {
            return Err(TypeError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("\"{}\" is out of range for type double precision", &self.text[i..i + len]),
            ));
        }
        Ok((value, self.skip_space(i + len)))
    }

    /// `pair_decode`: `x,y` or `(x,y)`.
    fn pair(&self, i: usize) -> Result<(Point, usize), TypeError> {
        let mut i = self.skip_space(i);
        let delim = self.at(i) == b'(';
        i += usize::from(delim);
        let (x, i) = self.single(i)?;
        let i = self.expect(i, b',')?;
        let (y, mut i) = self.single(i)?;
        if delim {
            i = self.skip_space(self.expect(i, b')')?);
        }
        Ok((Point { x, y }, i))
    }

    /// `path_decode`: `npts` points, in `[` and `]` for an open path when `open` allows it, or in parentheses. Gives the points, true for an open path, and the index after them.
    fn path(
        &self,
        i: usize,
        open: bool,
        npts: usize,
    ) -> Result<(Vec<Point>, bool, usize), TypeError> {
        let mut i = self.skip_space(i);
        let mut depth = 0;
        let is_open = self.at(i) == b'[';
        if is_open {
            if !open {
                return Err(self.syntax());
            }
            depth += 1;
            i += 1;
        } else if self.at(i) == b'(' {
            let cp = self.skip_space(i + 1);
            if self.at(cp) == b'(' || !self.text.as_bytes()[i + 1..].contains(&b'(') {
                depth += 1;
                i = cp;
            }
        }
        let mut points = Vec::with_capacity(npts.min(self.text.len()));
        for _ in 0..npts {
            let (p, next) = self.pair(i)?;
            points.push(p);
            i = next + usize::from(self.at(next) == b',');
        }
        while depth > 0 {
            if self.at(i) == b')' || self.at(i) == b']' && is_open && depth == 1 {
                depth -= 1;
                i = self.skip_space(i + 1);
            } else {
                return Err(self.syntax());
            }
        }
        Ok((points, is_open, i))
    }

    /// `pair_count`: the number of points of a path or a polygon, from the number of commas, which must be odd.
    fn pair_count(&self) -> Result<usize, TypeError> {
        let commas = self.text.bytes().filter(|&c| c == b',').count();
        if commas % 2 == 1 { Ok(commas.div_ceil(2)) } else { Err(self.syntax()) }
    }
}

/// The bytes of the binary form of the points.
fn put_points(points: &[Point], out: &mut Vec<u8>) {
    for p in points {
        out.extend_from_slice(&p.x.to_be_bytes());
        out.extend_from_slice(&p.y.to_be_bytes());
    }
}

/// The count of points of a path or a polygon as an `int4`.
fn put_count(n: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(&i32::try_from(n).unwrap_or(i32::MAX).to_be_bytes());
}

/// The corners of a box with the high corner first, as `box_in` and `box_recv` order them.
fn box_corners(mut high: Point, mut low: Point) -> [Point; 2] {
    if float8_lt(high.x, low.x) {
        std::mem::swap(&mut high.x, &mut low.x);
    }
    if float8_lt(high.y, low.y) {
        std::mem::swap(&mut high.y, &mut low.y);
    }
    [high, low]
}

/// `22P02` or `22P03`, `invalid line specification: A and B cannot both be zero`.
fn check_line(a: f64, b: f64, binary: bool) -> Result<(), TypeError> {
    if fp_zero(a) && fp_zero(b) {
        let state = if binary {
            SqlState::INVALID_BINARY_REPRESENTATION
        } else {
            SqlState::INVALID_TEXT_REPRESENTATION
        };
        return Err(TypeError::new(
            state,
            "invalid line specification: A and B cannot both be zero".to_string(),
        ));
    }
    Ok(())
}

/// The text input functions of the geometric types, from `point_in` to `circle_in`. The value is the bytes of the binary form.
///
/// # Errors
///
/// `22P02` for text that is not valid, `22003` for a number out of the range of `double precision`, and the errors of the slope of a `line` given by two points.
pub fn geo_in(kind: GeoKind, text: &str) -> Result<Vec<u8>, TypeError> {
    let r = Reader { text, kind };
    let mut out = Vec::new();
    match kind {
        GeoKind::Point => {
            let (p, i) = r.pair(0)?;
            r.end(i)?;
            put_points(&[p], &mut out);
        }
        GeoKind::Lseg => {
            let (points, _, i) = r.path(0, true, 2)?;
            r.end(i)?;
            put_points(&points, &mut out);
        }
        GeoKind::Box => {
            let (points, _, i) = r.path(0, false, 2)?;
            r.end(i)?;
            put_points(&box_corners(points[0], points[1]), &mut out);
        }
        GeoKind::Path => {
            let npts = r.pair_count()?;
            let mut i = r.skip_space(0);
            // A single opening parenthesis, as in `(1,2,3,4)`, holds the points.
            let single = r.at(i) == b'(' && !text.as_bytes()[i + 1..].contains(&b'(');
            i += usize::from(single);
            let (points, open, mut i) = r.path(i, true, npts)?;
            if single {
                i = r.skip_space(r.expect(i, b')')?);
            }
            r.end(i)?;
            out.push(u8::from(!open));
            put_count(points.len(), &mut out);
            put_points(&points, &mut out);
        }
        GeoKind::Polygon => {
            let npts = r.pair_count()?;
            let (points, _, i) = r.path(0, false, npts)?;
            r.end(i)?;
            put_count(points.len(), &mut out);
            put_points(&points, &mut out);
        }
        GeoKind::Line => {
            let i = r.skip_space(0);
            let coefficients = if r.at(i) == b'{' {
                let (a, i) = r.single(i + 1)?;
                let (b, i) = r.single(r.expect(i, b',')?)?;
                let (c, i) = r.single(r.expect(i, b',')?)?;
                r.end(r.skip_space(r.expect(i, b'}')?))?;
                check_line(a, b, false)?;
                [a, b, c]
            } else {
                let (points, _, i) = r.path(i, true, 2)?;
                r.end(i)?;
                if point_eq(points[0], points[1]) {
                    return Err(TypeError::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        "invalid line specification: must be two distinct points".to_string(),
                    ));
                }
                line_construct(points[0], point_sl(points[0], points[1])?)?
            };
            for v in coefficients {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        GeoKind::Circle => {
            let mut i = r.skip_space(0);
            let mut depth = 0;
            if r.at(i) == b'<' {
                depth += 1;
                i += 1;
            } else if r.at(i) == b'(' {
                // Of two opening parentheses, the first one holds the circle.
                let cp = r.skip_space(i + 1);
                if r.at(cp) == b'(' {
                    depth += 1;
                    i = cp;
                }
            }
            let (center, i) = r.pair(i)?;
            let i = i + usize::from(r.at(i) == b',');
            let (radius, mut i) = r.single(i)?;
            if radius < 0.0 {
                return Err(r.syntax());
            }
            while depth > 0 {
                if r.at(i) == b')' || r.at(i) == b'>' && depth == 1 {
                    depth -= 1;
                    i = r.skip_space(i + 1);
                } else {
                    return Err(r.syntax());
                }
            }
            r.end(i)?;
            put_points(&[center], &mut out);
            out.extend_from_slice(&radius.to_be_bytes());
        }
    }
    Ok(out)
}

/// The numbers of the binary form from byte `from`.
fn numbers(bytes: &[u8], from: usize) -> impl Iterator<Item = f64> + '_ {
    bytes.get(from..).unwrap_or_default().as_chunks::<8>().0.iter().map(|c| f64::from_be_bytes(*c))
}

/// `pair_encode` in parentheses.
fn put_pair(x: f64, y: f64, digits: i32, out: &mut Vec<u8>) {
    out.push(b'(');
    float8_out(x, digits, out);
    out.push(b',');
    float8_out(y, digits, out);
    out.push(b')');
}

/// `path_encode`: the points from byte `from`, with commas between them.
fn put_path(bytes: &[u8], from: usize, digits: i32, out: &mut Vec<u8>) {
    let values: Vec<f64> = numbers(bytes, from).collect();
    for (n, p) in values.as_chunks::<2>().0.iter().enumerate() {
        if n > 0 {
            out.push(b',');
        }
        put_pair(p[0], p[1], digits, out);
    }
}

/// The text output functions of the geometric types, from `point_out` to `circle_out`. `digits` is `extra_float_digits`.
pub fn geo_out(kind: GeoKind, bytes: &[u8], digits: i32, out: &mut Vec<u8>) {
    match kind {
        GeoKind::Point | GeoKind::Box => put_path(bytes, 0, digits, out),
        GeoKind::Lseg => {
            out.push(b'[');
            put_path(bytes, 0, digits, out);
            out.push(b']');
        }
        GeoKind::Path => {
            let closed = bytes.first().is_some_and(|&c| c != 0);
            out.push(if closed { b'(' } else { b'[' });
            put_path(bytes, 5, digits, out);
            out.push(if closed { b')' } else { b']' });
        }
        GeoKind::Polygon => {
            out.push(b'(');
            put_path(bytes, 4, digits, out);
            out.push(b')');
        }
        GeoKind::Line => {
            out.push(b'{');
            for (n, v) in numbers(bytes, 0).enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                float8_out(v, digits, out);
            }
            out.push(b'}');
        }
        GeoKind::Circle => {
            let v: Vec<f64> = numbers(bytes, 0).collect();
            if let [x, y, r] = v[..] {
                out.push(b'<');
                put_pair(x, y, digits, out);
                out.push(b',');
                float8_out(r, digits, out);
                out.push(b'>');
            }
        }
    }
}

/// `n` points of the binary form.
fn recv_points(recv: &mut Recv<'_>, n: usize) -> Result<Vec<Point>, TypeError> {
    let mut points = Vec::with_capacity(n.min(recv.remaining() / 16));
    for _ in 0..n {
        let x = recv.f64()?;
        points.push(Point { x, y: recv.f64()? });
    }
    Ok(points)
}

/// The count of points of a path or a polygon in the binary form, which must be more than 0 and fit in an `int` with the header.
fn recv_count(recv: &mut Recv<'_>, header: usize, name: &str) -> Result<usize, TypeError> {
    let n = recv.i32()?;
    let limit = (0x7fff_ffff - header) / 16;
    usize::try_from(n).ok().filter(|&n| n > 0 && n < limit).ok_or_else(|| {
        TypeError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            format!("invalid number of points in external \"{name}\" value"),
        )
    })
}

/// The receive functions of the geometric types, from `point_recv` to `circle_recv`. The corners of a box are put in order and the flag of a path is 0 or 1.
///
/// # Errors
///
/// `22P03` for a `line` whose `A` and `B` are zero, a `circle` with a negative radius, or a path or a polygon with a count of points that is not valid, and `08P01` for too few bytes.
pub fn geo_recv(kind: GeoKind, recv: &mut Recv<'_>) -> Result<Vec<u8>, TypeError> {
    let mut out = Vec::new();
    match kind {
        GeoKind::Point => put_points(&recv_points(recv, 1)?, &mut out),
        GeoKind::Lseg => put_points(&recv_points(recv, 2)?, &mut out),
        GeoKind::Box => {
            let p = recv_points(recv, 2)?;
            put_points(&box_corners(p[0], p[1]), &mut out);
        }
        GeoKind::Path => {
            let closed = recv.byte()? != 0;
            let n = recv_count(recv, 16, "path")?;
            let points = recv_points(recv, n)?;
            out.push(u8::from(closed));
            put_count(n, &mut out);
            put_points(&points, &mut out);
        }
        GeoKind::Polygon => {
            let n = recv_count(recv, 40, "polygon")?;
            let points = recv_points(recv, n)?;
            put_count(n, &mut out);
            put_points(&points, &mut out);
        }
        GeoKind::Line => {
            let (a, b, c) = (recv.f64()?, recv.f64()?, recv.f64()?);
            check_line(a, b, true)?;
            for v in [a, b, c] {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        GeoKind::Circle => {
            let center = recv_points(recv, 1)?;
            let radius = recv.f64()?;
            if radius < 0.0 {
                return Err(TypeError::new(
                    SqlState::INVALID_BINARY_REPRESENTATION,
                    "invalid radius in external \"circle\" value".to_string(),
                ));
            }
            put_points(&center, &mut out);
            out.extend_from_slice(&radius.to_be_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(kind: GeoKind, s: &str) -> String {
        let bytes = geo_in(kind, s).unwrap();
        let mut out = Vec::new();
        geo_out(kind, &bytes, 1, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn error(kind: GeoKind, s: &str) -> String {
        geo_in(kind, s).unwrap_err().message
    }

    /// The values and the errors of PostgreSQL 19 for the same text.
    #[test]
    fn text_forms() {
        assert_eq!(text(GeoKind::Point, " ( 1.5 , -2 ) "), "(1.5,-2)");
        assert_eq!(text(GeoKind::Point, "(NaN,Infinity)"), "(NaN,Infinity)");
        assert_eq!(error(GeoKind::Point, "(1,2"), "invalid input syntax for type point: \"(1,2\"");
        assert_eq!(
            error(GeoKind::Point, "(1e400,2)"),
            "\"1e400\" is out of range for type double precision"
        );
        assert_eq!(text(GeoKind::Lseg, "(1,2,3,4)"), "[(1,2),(3,4)]");
        assert_eq!(text(GeoKind::Box, "(NaN,1),(2,3)"), "(NaN,3),(2,1)");
        assert!(geo_in(GeoKind::Box, "[(1,2),(3,4)]").is_err());
        assert_eq!(text(GeoKind::Line, "(1,2),(3,7)"), "{2.5,-1,-0.5}");
        assert_eq!(text(GeoKind::Line, "(0,0),(0,5)"), "{-1,0,0}");
        assert_eq!(
            error(GeoKind::Line, "{1e-7,0,1}"),
            "invalid line specification: A and B cannot both be zero"
        );
        assert_eq!(text(GeoKind::Path, "(1,2,3,4)"), "((1,2),(3,4))");
        assert_eq!(text(GeoKind::Path, "[(1,2)]"), "[(1,2)]");
        assert!(geo_in(GeoKind::Path, "(1,2),(3,4)]").is_err());
        assert_eq!(text(GeoKind::Polygon, "1,2,3,4,5,6"), "((1,2),(3,4),(5,6))");
        assert_eq!(text(GeoKind::Circle, "((1,2),3>"), "<(1,2),3>");
        assert!(geo_in(GeoKind::Circle, "(1,2,3)").is_err());
        assert!(geo_in(GeoKind::Circle, "<(1,2),-1>").is_err());
    }

    #[test]
    fn binary_forms() {
        let bytes = geo_in(GeoKind::Path, "[(1,2),(3,4)]").unwrap();
        assert_eq!(bytes.len(), 37);
        assert_eq!(geo_recv(GeoKind::Path, &mut Recv::new(&bytes)).unwrap(), bytes);
        let mut bad = bytes.clone();
        bad[1..5].copy_from_slice(&0i32.to_be_bytes());
        assert!(geo_recv(GeoKind::Path, &mut Recv::new(&bad)).is_err());
        let line = [0.0f64, 0.0, 1.0].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>();
        let e = geo_recv(GeoKind::Line, &mut Recv::new(&line)).unwrap_err();
        assert_eq!(e.sqlstate.as_str(), "22P03");
    }
}
