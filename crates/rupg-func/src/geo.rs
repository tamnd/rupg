//! The functions and operators of `point`, `lseg`, `line`, `box`, `path`, `polygon` and `circle` from `geo_ops.c`: the constructors, the casts, the comparisons, the arithmetic, the distances, the closest points, the intersections and the containment tests. A value is the bytes of its binary form in [`Value::Bytea`], as `rupg-types` reads it.

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::math::{division_by_zero, overflow, underflow};
use crate::{Call, Kernel, bad_value};

/// `EPSILON`: two numbers that differ by no more than this are equal in the fuzzy comparisons of `geo_decls.h`.
const EPSILON: f64 = 1.0e-6;

/// The kernel of a geometric function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "construct_point" => |_, a| Ok(point_value(Point { x: float8(a, 0)?, y: float8(a, 1)? })),
        "point_left" => |_, a| points(a, |p, q| Ok(fp_lt(p.x, q.x))),
        "point_right" => |_, a| points(a, |p, q| Ok(fp_gt(p.x, q.x))),
        "point_above" => |_, a| points(a, |p, q| Ok(fp_gt(p.y, q.y))),
        "point_below" => |_, a| points(a, |p, q| Ok(fp_lt(p.y, q.y))),
        "point_vert" => |_, a| points(a, |p, q| Ok(fp_eq(p.x, q.x))),
        "point_horiz" => |_, a| points(a, |p, q| Ok(fp_eq(p.y, q.y))),
        "point_eq" => |_, a| points(a, |p, q| Ok(point_eq(p, q))),
        "point_ne" => |_, a| points(a, |p, q| Ok(!point_eq(p, q))),
        "point_distance" => |_, a| Ok(Value::Float8(point_dt(point(a, 0)?, point(a, 1)?)?)),
        "point_slope" => |_, a| Ok(Value::Float8(point_sl(point(a, 0)?, point(a, 1)?)?)),
        "point_add" => |_, a| Ok(point_value(point_add(point(a, 0)?, point(a, 1)?)?)),
        "point_sub" => |_, a| Ok(point_value(point_sub(point(a, 0)?, point(a, 1)?)?)),
        "point_mul" => |_, a| Ok(point_value(point_mul(point(a, 0)?, point(a, 1)?)?)),
        "point_div" => |_, a| Ok(point_value(point_div(point(a, 0)?, point(a, 1)?)?)),
        "points_box" => |_, a| Ok(box_value(box_construct(point(a, 0)?, point(a, 1)?))),
        "point_box" => |_, a| {
            let p = point(a, 0)?;
            Ok(box_value(Rect { high: p, low: p }))
        },
        "box_same" => {
            |_, a| boxes(a, |b, c| Ok(point_eq(b.high, c.high) && point_eq(b.low, c.low)))
        }
        "box_overlap" => |_, a| boxes(a, |b, c| Ok(box_ov(b, c))),
        "box_left" => |_, a| boxes(a, |b, c| Ok(fp_lt(b.high.x, c.low.x))),
        "box_overleft" => |_, a| boxes(a, |b, c| Ok(fp_le(b.high.x, c.high.x))),
        "box_right" => |_, a| boxes(a, |b, c| Ok(fp_gt(b.low.x, c.high.x))),
        // `concat!` keeps the PostgreSQL name out of the check of typos.
        concat!("box_over", "right") => |_, a| boxes(a, |b, c| Ok(fp_ge(b.low.x, c.low.x))),
        "box_below" => |_, a| boxes(a, |b, c| Ok(fp_lt(b.high.y, c.low.y))),
        "box_overbelow" => |_, a| boxes(a, |b, c| Ok(fp_le(b.high.y, c.high.y))),
        "box_above" => |_, a| boxes(a, |b, c| Ok(fp_gt(b.low.y, c.high.y))),
        "box_overabove" => |_, a| boxes(a, |b, c| Ok(fp_ge(b.low.y, c.low.y))),
        "box_contained" => |_, a| boxes(a, |b, c| Ok(box_contain_box(c, b))),
        "box_contain" => |_, a| boxes(a, |b, c| Ok(box_contain_box(b, c))),
        "box_below_eq" => |_, a| boxes(a, |b, c| Ok(fp_le(b.high.y, c.low.y))),
        "box_above_eq" => |_, a| boxes(a, |b, c| Ok(fp_ge(b.low.y, c.high.y))),
        "box_lt" => |_, a| boxes(a, |b, c| Ok(fp_lt(box_ar(b)?, box_ar(c)?))),
        "box_gt" => |_, a| boxes(a, |b, c| Ok(fp_gt(box_ar(b)?, box_ar(c)?))),
        "box_eq" => |_, a| boxes(a, |b, c| Ok(fp_eq(box_ar(b)?, box_ar(c)?))),
        "box_le" => |_, a| boxes(a, |b, c| Ok(fp_le(box_ar(b)?, box_ar(c)?))),
        "box_ge" => |_, a| boxes(a, |b, c| Ok(fp_ge(box_ar(b)?, box_ar(c)?))),
        "box_contain_pt" => |_, a| Ok(Value::Bool(box_contain_point(rect(a, 0)?, point(a, 1)?))),
        "box_area" => |_, a| Ok(Value::Float8(box_ar(rect(a, 0)?)?)),
        "box_width" => |_, a| Ok(Value::Float8(box_wd(rect(a, 0)?)?)),
        "box_height" => |_, a| Ok(Value::Float8(box_ht(rect(a, 0)?)?)),
        "box_distance" => {
            |_, a| Ok(Value::Float8(point_dt(box_cn(rect(a, 0)?)?, box_cn(rect(a, 1)?)?)?))
        }
        "box_center" => |_, a| Ok(point_value(box_cn(rect(a, 0)?)?)),
        "box_intersect" => box_intersect,
        "box_diagonal" => |_, a| {
            let b = rect(a, 0)?;
            Ok(lseg_value(Lseg { p: [b.high, b.low] }))
        },
        "box_add" => |_, a| box_translate(a, point_add),
        "box_sub" => |_, a| box_translate(a, point_sub),
        "box_mul" => |_, a| box_scale(a, point_mul),
        "box_div" => |_, a| box_scale(a, point_div),
        "boxes_bound_box" => boxes_bound_box,
        "lseg_construct" => |_, a| Ok(lseg_value(Lseg { p: [point(a, 0)?, point(a, 1)?] })),
        "lseg_length" => |_, a| {
            let s = lseg(a, 0)?;
            Ok(Value::Float8(point_dt(s.p[0], s.p[1])?))
        },
        "lseg_intersect" => |_, a| lsegs(a, |s, t| Ok(lseg_interpt_lseg(s, t)?.is_some())),
        "lseg_parallel" => |_, a| lsegs(a, |s, t| Ok(fp_eq(lseg_sl(s)?, lseg_sl(t)?))),
        "lseg_perp" => |_, a| lsegs(a, |s, t| Ok(fp_eq(lseg_sl(s)?, point_invsl(t.p[0], t.p[1])?))),
        "lseg_vertical" => |_, a| {
            let s = lseg(a, 0)?;
            Ok(Value::Bool(fp_eq(s.p[0].x, s.p[1].x)))
        },
        "lseg_horizontal" => |_, a| {
            let s = lseg(a, 0)?;
            Ok(Value::Bool(fp_eq(s.p[0].y, s.p[1].y)))
        },
        "lseg_eq" => {
            |_, a| lsegs(a, |s, t| Ok(point_eq(s.p[0], t.p[0]) && point_eq(s.p[1], t.p[1])))
        }
        "lseg_ne" => {
            |_, a| lsegs(a, |s, t| Ok(!point_eq(s.p[0], t.p[0]) || !point_eq(s.p[1], t.p[1])))
        }
        "lseg_lt" => |_, a| lsegs(a, |s, t| Ok(fp_lt(lseg_len(s)?, lseg_len(t)?))),
        "lseg_le" => |_, a| lsegs(a, |s, t| Ok(fp_le(lseg_len(s)?, lseg_len(t)?))),
        "lseg_gt" => |_, a| lsegs(a, |s, t| Ok(fp_gt(lseg_len(s)?, lseg_len(t)?))),
        "lseg_ge" => |_, a| lsegs(a, |s, t| Ok(fp_ge(lseg_len(s)?, lseg_len(t)?))),
        "lseg_distance" => |_, a| Ok(Value::Float8(lseg_closept_lseg(lseg(a, 0)?, lseg(a, 1)?)?.0)),
        "lseg_center" => |_, a| {
            let s = lseg(a, 0)?;
            let x = float8_div(float8_pl(s.p[0].x, s.p[1].x)?, 2.0)?;
            let y = float8_div(float8_pl(s.p[0].y, s.p[1].y)?, 2.0)?;
            Ok(point_value(Point { x, y }))
        },
        "lseg_interpt" => |_, a| Ok(maybe_point(lseg_interpt_lseg(lseg(a, 0)?, lseg(a, 1)?)?)),
        "line_construct_pp" => line_construct_pp,
        "line_intersect" => |_, a| lines(a, |l, m| Ok(line_interpt_line(l, m)?.is_some())),
        "line_parallel" => |_, a| lines(a, |l, m| Ok(line_interpt_line(l, m)?.is_none())),
        "line_perp" => |_, a| lines(a, line_perp),
        "line_vertical" => |_, a| Ok(Value::Bool(fp_zero(line(a, 0)?.b))),
        "line_horizontal" => |_, a| Ok(Value::Bool(fp_zero(line(a, 0)?.a))),
        "line_eq" => |_, a| lines(a, line_eq),
        "line_distance" => line_distance,
        "line_interpt" => |_, a| Ok(maybe_point(line_interpt_line(line(a, 0)?, line(a, 1)?)?)),
        "dist_pl" => |_, a| Ok(Value::Float8(line_closept_point(line(a, 1)?, point(a, 0)?)?.0)),
        "dist_lp" => |_, a| Ok(Value::Float8(line_closept_point(line(a, 0)?, point(a, 1)?)?.0)),
        "dist_ps" => |_, a| Ok(Value::Float8(lseg_closept_point(lseg(a, 1)?, point(a, 0)?)?.0)),
        "dist_sp" => |_, a| Ok(Value::Float8(lseg_closept_point(lseg(a, 0)?, point(a, 1)?)?.0)),
        "dist_pb" => |_, a| Ok(Value::Float8(box_closept_point(rect(a, 1)?, point(a, 0)?)?.0)),
        "dist_bp" => |_, a| Ok(Value::Float8(box_closept_point(rect(a, 0)?, point(a, 1)?)?.0)),
        "dist_sl" => |_, a| Ok(Value::Float8(lseg_closept_line(lseg(a, 0)?, line(a, 1)?)?.0)),
        "dist_ls" => |_, a| Ok(Value::Float8(lseg_closept_line(lseg(a, 1)?, line(a, 0)?)?.0)),
        "dist_sb" => |_, a| Ok(Value::Float8(box_closept_lseg(rect(a, 1)?, lseg(a, 0)?, false)?.0)),
        "dist_bs" => |_, a| Ok(Value::Float8(box_closept_lseg(rect(a, 0)?, lseg(a, 1)?, false)?.0)),
        "close_pl" => |_, a| Ok(closest(line_closept_point(line(a, 1)?, point(a, 0)?)?)),
        "close_ps" => |_, a| Ok(closest(lseg_closept_point(lseg(a, 1)?, point(a, 0)?)?)),
        "close_lseg" => close_lseg,
        "close_pb" => |_, a| Ok(closest(box_closept_point(rect(a, 1)?, point(a, 0)?)?)),
        "close_ls" => close_ls,
        "close_sb" => |_, a| Ok(closest(box_closept_lseg(rect(a, 1)?, lseg(a, 0)?, true)?)),
        "inter_sl" => {
            |_, a| Ok(Value::Bool(lseg_interpt_line(lseg(a, 0)?, line(a, 1)?)?.is_some()))
        }
        "inter_sb" => |_, a| Ok(Value::Bool(box_interpt_lseg(rect(a, 1)?, lseg(a, 0)?)?)),
        "inter_lb" => inter_lb,
        "on_pl" => |_, a| Ok(Value::Bool(line_contain_point(line(a, 1)?, point(a, 0)?)?)),
        "on_ps" => |_, a| Ok(Value::Bool(lseg_contain_point(lseg(a, 1)?, point(a, 0)?)?)),
        "on_pb" => |_, a| Ok(Value::Bool(box_contain_point(rect(a, 1)?, point(a, 0)?))),
        "on_sl" => |_, a| {
            let (s, l) = (lseg(a, 0)?, line(a, 1)?);
            Ok(Value::Bool(line_contain_point(l, s.p[0])? && line_contain_point(l, s.p[1])?))
        },
        "on_sb" => |_, a| {
            let (s, b) = (lseg(a, 0)?, rect(a, 1)?);
            Ok(Value::Bool(box_contain_point(b, s.p[0]) && box_contain_point(b, s.p[1])))
        },
        "path_area" => path_area,
        "path_n_lt" => |_, a| Ok(Value::Bool(path(a, 0)?.p.len() < path(a, 1)?.p.len())),
        "path_n_gt" => |_, a| Ok(Value::Bool(path(a, 0)?.p.len() > path(a, 1)?.p.len())),
        "path_n_eq" => |_, a| Ok(Value::Bool(path(a, 0)?.p.len() == path(a, 1)?.p.len())),
        "path_n_le" => |_, a| Ok(Value::Bool(path(a, 0)?.p.len() <= path(a, 1)?.p.len())),
        "path_n_ge" => |_, a| Ok(Value::Bool(path(a, 0)?.p.len() >= path(a, 1)?.p.len())),
        "path_isclosed" => |_, a| Ok(Value::Bool(path(a, 0)?.closed)),
        "path_isopen" => |_, a| Ok(Value::Bool(!path(a, 0)?.closed)),
        "path_npoints" => |_, a| Ok(Value::Int4(count(&path(a, 0)?.p))),
        "path_close" => |_, a| Ok(path_value(true, &path(a, 0)?.p)),
        "path_open" => |_, a| Ok(path_value(false, &path(a, 0)?.p)),
        "path_inter" => path_inter,
        "path_distance" => path_distance,
        "path_length" => |_, a| {
            let mut result = 0.0;
            for s in path_segments(&path(a, 0)?) {
                result = float8_pl(result, point_dt(s.p[0], s.p[1])?)?;
            }
            Ok(Value::Float8(result))
        },
        "path_add" => |_, a| {
            let (p1, p2) = (path(a, 0)?, path(a, 1)?);
            if p1.closed || p2.closed {
                return Ok(Value::Null);
            }
            Ok(path_value(false, &[p1.p, p2.p].concat()))
        },
        "path_add_pt" => |_, a| path_translate(a, point_add),
        "path_sub_pt" => |_, a| path_translate(a, point_sub),
        "path_mul_pt" => |_, a| path_translate(a, point_mul),
        "path_div_pt" => |_, a| path_translate(a, point_div),
        "path_poly" => |_, a| {
            let p = path(a, 0)?;
            if !p.closed {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "open path cannot be converted to polygon",
                ));
            }
            Ok(poly_value(&p.p))
        },
        "poly_path" => |_, a| Ok(path_value(true, &polygon(a, 0)?)),
        "dist_ppath" => |_, a| Ok(Value::Float8(dist_ppath(point(a, 0)?, &path(a, 1)?)?)),
        "dist_pathp" => |_, a| Ok(Value::Float8(dist_ppath(point(a, 1)?, &path(a, 0)?)?)),
        "on_ppath" => |_, a| Ok(Value::Bool(on_ppath(point(a, 0)?, &path(a, 1)?)?)),
        "poly_left" => |_, a| polys(a, |b, c| b.high.x < c.low.x),
        "poly_overleft" => |_, a| polys(a, |b, c| b.high.x <= c.high.x),
        "poly_right" => |_, a| polys(a, |b, c| b.low.x > c.high.x),
        concat!("poly_over", "right") => |_, a| polys(a, |b, c| b.low.x >= c.low.x),
        "poly_below" => |_, a| polys(a, |b, c| b.high.y < c.low.y),
        "poly_overbelow" => |_, a| polys(a, |b, c| b.high.y <= c.high.y),
        "poly_above" => |_, a| polys(a, |b, c| b.low.y > c.high.y),
        "poly_overabove" => |_, a| polys(a, |b, c| b.low.y >= c.low.y),
        "poly_same" => |_, a| {
            let (p, q) = (polygon(a, 0)?, polygon(a, 1)?);
            Ok(Value::Bool(p.len() == q.len() && plist_same(&p, &q)))
        },
        "poly_overlap" => |_, a| Ok(Value::Bool(poly_overlap(&polygon(a, 0)?, &polygon(a, 1)?)?)),
        "poly_contain" => {
            |_, a| Ok(Value::Bool(poly_contain_poly(&polygon(a, 0)?, &polygon(a, 1)?)?))
        }
        "poly_contained" => {
            |_, a| Ok(Value::Bool(poly_contain_poly(&polygon(a, 1)?, &polygon(a, 0)?)?))
        }
        "poly_contain_pt" => {
            |_, a| Ok(Value::Bool(point_inside(point(a, 1)?, &polygon(a, 0)?)? != 0))
        }
        "pt_contained_poly" => {
            |_, a| Ok(Value::Bool(point_inside(point(a, 0)?, &polygon(a, 1)?)? != 0))
        }
        "poly_distance" => poly_distance,
        "poly_npoints" => |_, a| Ok(Value::Int4(count(&polygon(a, 0)?))),
        "poly_center" => |_, a| Ok(point_value(poly_to_circle(&polygon(a, 0)?)?.center)),
        "poly_box" => |_, a| Ok(box_value(bound_box(&polygon(a, 0)?))),
        "box_poly" => |_, a| {
            let b = rect(a, 0)?;
            let (h, l) = (b.high, b.low);
            let corners = [l, Point { x: l.x, y: h.y }, h, Point { x: h.x, y: l.y }];
            Ok(poly_value(&corners))
        },
        "poly_circle" => |_, a| Ok(circle_value(poly_to_circle(&polygon(a, 0)?)?)),
        "dist_ppoly" => |_, a| Ok(Value::Float8(dist_ppoly(point(a, 0)?, &polygon(a, 1)?)?)),
        "dist_polyp" => |_, a| Ok(Value::Float8(dist_ppoly(point(a, 1)?, &polygon(a, 0)?)?)),
        "dist_cpoly" => |_, a| Ok(Value::Float8(dist_cpoly(circle(a, 0)?, &polygon(a, 1)?)?)),
        "dist_polyc" => |_, a| Ok(Value::Float8(dist_cpoly(circle(a, 1)?, &polygon(a, 0)?)?)),
        "circle_same" => |_, a| {
            circles(a, |c, d| {
                let radius = (c.radius.is_nan() && d.radius.is_nan()) || fp_eq(c.radius, d.radius);
                Ok(radius && point_eq(c.center, d.center))
            })
        },
        "circle_overlap" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_le(point_dt(c.center, d.center)?, float8_pl(c.radius, d.radius)?))
            })
        },
        "circle_overleft" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_le(float8_pl(c.center.x, c.radius)?, float8_pl(d.center.x, d.radius)?))
            })
        },
        "circle_left" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_lt(float8_pl(c.center.x, c.radius)?, float8_mi(d.center.x, d.radius)?))
            })
        },
        "circle_right" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_gt(float8_mi(c.center.x, c.radius)?, float8_pl(d.center.x, d.radius)?))
            })
        },
        concat!("circle_over", "right") => |_, a| {
            circles(a, |c, d| {
                Ok(fp_ge(float8_mi(c.center.x, c.radius)?, float8_mi(d.center.x, d.radius)?))
            })
        },
        "circle_contained" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_le(point_dt(c.center, d.center)?, float8_mi(d.radius, c.radius)?))
            })
        },
        "circle_contain" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_le(point_dt(c.center, d.center)?, float8_mi(c.radius, d.radius)?))
            })
        },
        "circle_below" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_lt(float8_pl(c.center.y, c.radius)?, float8_mi(d.center.y, d.radius)?))
            })
        },
        "circle_above" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_gt(float8_mi(c.center.y, c.radius)?, float8_pl(d.center.y, d.radius)?))
            })
        },
        "circle_overbelow" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_le(float8_pl(c.center.y, c.radius)?, float8_pl(d.center.y, d.radius)?))
            })
        },
        "circle_overabove" => |_, a| {
            circles(a, |c, d| {
                Ok(fp_ge(float8_mi(c.center.y, c.radius)?, float8_mi(d.center.y, d.radius)?))
            })
        },
        "circle_eq" => |_, a| circles(a, |c, d| Ok(fp_eq(circle_ar(c)?, circle_ar(d)?))),
        "circle_ne" => |_, a| circles(a, |c, d| Ok(fp_ne(circle_ar(c)?, circle_ar(d)?))),
        "circle_lt" => |_, a| circles(a, |c, d| Ok(fp_lt(circle_ar(c)?, circle_ar(d)?))),
        "circle_gt" => |_, a| circles(a, |c, d| Ok(fp_gt(circle_ar(c)?, circle_ar(d)?))),
        "circle_le" => |_, a| circles(a, |c, d| Ok(fp_le(circle_ar(c)?, circle_ar(d)?))),
        "circle_ge" => |_, a| circles(a, |c, d| Ok(fp_ge(circle_ar(c)?, circle_ar(d)?))),
        "circle_add_pt" => |_, a| {
            let (c, p) = (circle(a, 0)?, point(a, 1)?);
            Ok(circle_value(Circle { center: point_add(c.center, p)?, radius: c.radius }))
        },
        "circle_sub_pt" => |_, a| {
            let (c, p) = (circle(a, 0)?, point(a, 1)?);
            Ok(circle_value(Circle { center: point_sub(c.center, p)?, radius: c.radius }))
        },
        "circle_mul_pt" => |_, a| {
            let (c, p) = (circle(a, 0)?, point(a, 1)?);
            let center = point_mul(c.center, p)?;
            Ok(circle_value(Circle { center, radius: float8_mul(c.radius, p.x.hypot(p.y))? }))
        },
        "circle_div_pt" => |_, a| {
            let (c, p) = (circle(a, 0)?, point(a, 1)?);
            let center = point_div(c.center, p)?;
            Ok(circle_value(Circle { center, radius: float8_div(c.radius, p.x.hypot(p.y))? }))
        },
        "circle_area" => |_, a| Ok(Value::Float8(circle_ar(circle(a, 0)?)?)),
        "circle_diameter" => |_, a| Ok(Value::Float8(float8_mul(circle(a, 0)?.radius, 2.0)?)),
        "circle_radius" => |_, a| Ok(Value::Float8(circle(a, 0)?.radius)),
        "circle_distance" => |_, a| {
            let (c, d) = (circle(a, 0)?, circle(a, 1)?);
            let result = float8_mi(point_dt(c.center, d.center)?, float8_pl(c.radius, d.radius)?)?;
            Ok(Value::Float8(if result < 0.0 { 0.0 } else { result }))
        },
        "circle_contain_pt" => {
            |_, a| Ok(Value::Bool(circle_contain_point(circle(a, 0)?, point(a, 1)?)?))
        }
        "pt_contained_circle" => {
            |_, a| Ok(Value::Bool(circle_contain_point(circle(a, 1)?, point(a, 0)?)?))
        }
        "dist_pc" => |_, a| Ok(Value::Float8(dist_pc(point(a, 0)?, circle(a, 1)?)?)),
        "dist_cpoint" => |_, a| Ok(Value::Float8(dist_pc(point(a, 1)?, circle(a, 0)?)?)),
        "circle_center" => |_, a| Ok(point_value(circle(a, 0)?.center)),
        "cr_circle" => {
            |_, a| Ok(circle_value(Circle { center: point(a, 0)?, radius: float8(a, 1)? }))
        }
        "circle_box" => |_, a| {
            let c = circle(a, 0)?;
            let delta = float8_div(c.radius, 2.0_f64.sqrt())?;
            let hx = float8_pl(c.center.x, delta)?;
            let lx = float8_mi(c.center.x, delta)?;
            let hy = float8_pl(c.center.y, delta)?;
            let ly = float8_mi(c.center.y, delta)?;
            Ok(box_value(Rect { high: Point { x: hx, y: hy }, low: Point { x: lx, y: ly } }))
        },
        "box_circle" => |_, a| {
            let b = rect(a, 0)?;
            let x = float8_div(float8_pl(b.high.x, b.low.x)?, 2.0)?;
            let y = float8_div(float8_pl(b.high.y, b.low.y)?, 2.0)?;
            let center = Point { x, y };
            Ok(circle_value(Circle { center, radius: point_dt(center, b.high)? }))
        },
        "circle_poly" => |_, a| {
            let Some(Value::Int4(npts)) = a.first() else { return Err(bad_value()) };
            circle_poly(*npts, circle(a, 1)?)
        },
        "circle_to_poly" => |_, a| circle_poly(12, circle(a, 0)?),
        _ => return None,
    };
    Some(kernel)
}

/// A `point`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Point {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

/// An `lseg`, the segment between two points.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lseg {
    pub(crate) p: [Point; 2],
}

/// A `line`, the points where `Ax + By + C = 0`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Line {
    pub(crate) a: f64,
    pub(crate) b: f64,
    pub(crate) c: f64,
}

/// A `box`, with the high corner and the low corner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub(crate) high: Point,
    pub(crate) low: Point,
}

/// The numbers of the binary form of argument `n`, which must have `N` numbers.
pub(crate) fn floats<const N: usize>(args: &[Value], n: usize) -> Result<[f64; N]> {
    let Some(Value::Bytea(bytes)) = args.get(n) else { return Err(bad_value()) };
    let (chunks, rest) = bytes.as_chunks::<8>();
    if chunks.len() != N || !rest.is_empty() {
        return Err(bad_value());
    }
    Ok(std::array::from_fn(|i| f64::from_be_bytes(chunks[i])))
}

/// The `float8` of argument `n`.
fn float8(args: &[Value], n: usize) -> Result<f64> {
    match args.get(n) {
        Some(Value::Float8(v)) => Ok(*v),
        _ => Err(bad_value()),
    }
}

/// The `point` of argument `n`.
pub(crate) fn point(args: &[Value], n: usize) -> Result<Point> {
    let [x, y] = floats(args, n)?;
    Ok(Point { x, y })
}

/// The `lseg` of argument `n`.
pub(crate) fn lseg(args: &[Value], n: usize) -> Result<Lseg> {
    let [x0, y0, x1, y1] = floats(args, n)?;
    Ok(Lseg { p: [Point { x: x0, y: y0 }, Point { x: x1, y: y1 }] })
}

/// The `line` of argument `n`.
pub(crate) fn line(args: &[Value], n: usize) -> Result<Line> {
    let [a, b, c] = floats(args, n)?;
    Ok(Line { a, b, c })
}

/// The `box` of argument `n`.
pub(crate) fn rect(args: &[Value], n: usize) -> Result<Rect> {
    let [hx, hy, lx, ly] = floats(args, n)?;
    Ok(Rect { high: Point { x: hx, y: hy }, low: Point { x: lx, y: ly } })
}

/// A value with the binary form of these numbers.
pub(crate) fn floats_value(values: &[f64]) -> Value {
    Value::Bytea(values.iter().flat_map(|v| v.to_be_bytes()).collect())
}

pub(crate) fn point_value(p: Point) -> Value {
    floats_value(&[p.x, p.y])
}

pub(crate) fn lseg_value(s: Lseg) -> Value {
    floats_value(&[s.p[0].x, s.p[0].y, s.p[1].x, s.p[1].y])
}

pub(crate) fn line_value(l: Line) -> Value {
    floats_value(&[l.a, l.b, l.c])
}

pub(crate) fn box_value(b: Rect) -> Value {
    floats_value(&[b.high.x, b.high.y, b.low.x, b.low.y])
}

/// The point, or NULL for `None`.
fn maybe_point(p: Option<Point>) -> Value {
    p.map_or(Value::Null, point_value)
}

/// The closest point of a distance and a point, or NULL when the distance is NaN.
fn closest((dist, p): (f64, Point)) -> Value {
    if dist.is_nan() { Value::Null } else { point_value(p) }
}

/// A test of two points.
fn points(args: &[Value], test: fn(Point, Point) -> Result<bool>) -> Result<Value> {
    Ok(Value::Bool(test(point(args, 0)?, point(args, 1)?)?))
}

/// A test of two segments.
fn lsegs(args: &[Value], test: fn(Lseg, Lseg) -> Result<bool>) -> Result<Value> {
    Ok(Value::Bool(test(lseg(args, 0)?, lseg(args, 1)?)?))
}

/// A test of two lines.
fn lines(args: &[Value], test: fn(Line, Line) -> Result<bool>) -> Result<Value> {
    Ok(Value::Bool(test(line(args, 0)?, line(args, 1)?)?))
}

/// A test of two boxes.
fn boxes(args: &[Value], test: fn(Rect, Rect) -> Result<bool>) -> Result<Value> {
    Ok(Value::Bool(test(rect(args, 0)?, rect(args, 1)?)?))
}

/// `FPzero`.
pub(crate) fn fp_zero(a: f64) -> bool {
    a.abs() <= EPSILON
}

/// `FPeq`.
pub(crate) fn fp_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= EPSILON
}

/// `FPlt`.
pub(crate) fn fp_lt(a: f64, b: f64) -> bool {
    a + EPSILON < b
}

/// `FPle`.
pub(crate) fn fp_le(a: f64, b: f64) -> bool {
    a <= b + EPSILON
}

/// `FPgt`.
pub(crate) fn fp_gt(a: f64, b: f64) -> bool {
    a > b + EPSILON
}

/// `FPge`.
pub(crate) fn fp_ge(a: f64, b: f64) -> bool {
    a + EPSILON >= b
}

/// `float8_eq`: NaN is equal to NaN.
pub(crate) fn float8_eq(a: f64, b: f64) -> bool {
    if a.is_nan() { b.is_nan() } else { !b.is_nan() && a == b }
}

/// `float8_lt`: NaN is above all other numbers.
pub(crate) fn float8_lt(a: f64, b: f64) -> bool {
    !a.is_nan() && (b.is_nan() || a < b)
}

/// `float8_gt`.
pub(crate) fn float8_gt(a: f64, b: f64) -> bool {
    !b.is_nan() && (a.is_nan() || a > b)
}

/// `float8_min`.
pub(crate) fn float8_min(a: f64, b: f64) -> f64 {
    if float8_lt(a, b) { a } else { b }
}

/// `float8_max`.
pub(crate) fn float8_max(a: f64, b: f64) -> f64 {
    if float8_gt(a, b) { a } else { b }
}

/// `float8_pl`.
pub(crate) fn float8_pl(a: f64, b: f64) -> Result<f64> {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

/// `float8_mi`.
pub(crate) fn float8_mi(a: f64, b: f64) -> Result<f64> {
    let r = a - b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

/// `float8_mul`.
pub(crate) fn float8_mul(a: f64, b: f64) -> Result<f64> {
    let r = a * b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && a != 0.0 && b != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

/// `float8_div`.
pub(crate) fn float8_div(a: f64, b: f64) -> Result<f64> {
    if b == 0.0 && !a.is_nan() {
        return Err(division_by_zero());
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        return Err(underflow());
    }
    Ok(r)
}

/// `point_eq_point`: the fuzzy equality, or the exact equality when a coordinate is NaN.
pub(crate) fn point_eq(p: Point, q: Point) -> bool {
    if p.x.is_nan() || p.y.is_nan() || q.x.is_nan() || q.y.is_nan() {
        return float8_eq(p.x, q.x) && float8_eq(p.y, q.y);
    }
    fp_eq(p.x, q.x) && fp_eq(p.y, q.y)
}

/// `point_dt`: the distance between two points.
pub(crate) fn point_dt(p: Point, q: Point) -> Result<f64> {
    let x = float8_mi(p.x, q.x)?;
    let y = float8_mi(p.y, q.y)?;
    Ok(x.hypot(y))
}

/// `point_sl`: the slope of the line through two points, which is infinity for a vertical line.
pub(crate) fn point_sl(p: Point, q: Point) -> Result<f64> {
    if fp_eq(p.x, q.x) {
        return Ok(f64::INFINITY);
    }
    if fp_eq(p.y, q.y) {
        return Ok(0.0);
    }
    float8_div(float8_mi(p.y, q.y)?, float8_mi(p.x, q.x)?)
}

/// `point_invsl`: the slope of a line that is perpendicular to the line through two points.
pub(crate) fn point_invsl(p: Point, q: Point) -> Result<f64> {
    if fp_eq(p.x, q.x) {
        return Ok(0.0);
    }
    if fp_eq(p.y, q.y) {
        return Ok(f64::INFINITY);
    }
    float8_div(float8_mi(p.x, q.x)?, float8_mi(q.y, p.y)?)
}

/// `point_add_point`.
pub(crate) fn point_add(p: Point, q: Point) -> Result<Point> {
    Ok(Point { x: float8_pl(p.x, q.x)?, y: float8_pl(p.y, q.y)? })
}

/// `point_sub_point`.
pub(crate) fn point_sub(p: Point, q: Point) -> Result<Point> {
    Ok(Point { x: float8_mi(p.x, q.x)?, y: float8_mi(p.y, q.y)? })
}

/// `point_mul_point`: the product of two points as complex numbers, which turns and scales the first point.
pub(crate) fn point_mul(p: Point, q: Point) -> Result<Point> {
    let x = float8_mi(float8_mul(p.x, q.x)?, float8_mul(p.y, q.y)?)?;
    let y = float8_pl(float8_mul(p.x, q.y)?, float8_mul(p.y, q.x)?)?;
    Ok(Point { x, y })
}

/// `point_div_point`: the quotient of two points as complex numbers.
pub(crate) fn point_div(p: Point, q: Point) -> Result<Point> {
    let div = float8_pl(float8_mul(q.x, q.x)?, float8_mul(q.y, q.y)?)?;
    let x = float8_div(float8_pl(float8_mul(p.x, q.x)?, float8_mul(p.y, q.y)?)?, div)?;
    let y = float8_div(float8_mi(float8_mul(p.y, q.x)?, float8_mul(p.x, q.y)?)?, div)?;
    Ok(Point { x, y })
}

/// `line_construct`: the line through a point with a slope.
pub(crate) fn line_construct(pt: Point, m: f64) -> Result<Line> {
    if m.is_infinite() {
        return Ok(Line { a: -1.0, b: 0.0, c: pt.x });
    }
    if m == 0.0 {
        return Ok(Line { a: 0.0, b: -1.0, c: pt.y });
    }
    let c = float8_mi(pt.y, float8_mul(m, pt.x)?)?;
    // The subtraction can give -0, which PostgreSQL changes to 0.
    Ok(Line { a: m, b: -1.0, c: if c == 0.0 { 0.0 } else { c } })
}

/// `line_sl`: the slope of a line.
pub(crate) fn line_sl(l: Line) -> Result<f64> {
    if fp_zero(l.a) {
        return Ok(0.0);
    }
    if fp_zero(l.b) {
        return Ok(f64::INFINITY);
    }
    float8_div(l.a, -l.b)
}

/// `line_invsl`: the slope of a line that is perpendicular to a line.
pub(crate) fn line_invsl(l: Line) -> Result<f64> {
    if fp_zero(l.a) {
        return Ok(f64::INFINITY);
    }
    if fp_zero(l.b) {
        return Ok(0.0);
    }
    float8_div(l.b, l.a)
}

/// `line_interpt_line`: the point where two lines cross, or `None` for parallel lines.
pub(crate) fn line_interpt_line(l1: Line, l2: Line) -> Result<Option<Point>> {
    let (mut x, mut y);
    if !fp_zero(l1.b) {
        if fp_eq(l2.a, float8_mul(l1.a, float8_div(l2.b, l1.b)?)?) {
            return Ok(None);
        }
        x = float8_div(
            float8_mi(float8_mul(l1.b, l2.c)?, float8_mul(l2.b, l1.c)?)?,
            float8_mi(float8_mul(l1.a, l2.b)?, float8_mul(l2.a, l1.b)?)?,
        )?;
        y = float8_div(-float8_pl(float8_mul(l1.a, x)?, l1.c)?, l1.b)?;
    } else if !fp_zero(l2.b) {
        if fp_eq(l1.a, float8_mul(l2.a, float8_div(l1.b, l2.b)?)?) {
            return Ok(None);
        }
        x = float8_div(
            float8_mi(float8_mul(l2.b, l1.c)?, float8_mul(l1.b, l2.c)?)?,
            float8_mi(float8_mul(l2.a, l1.b)?, float8_mul(l1.a, l2.b)?)?,
        )?;
        y = float8_div(-float8_pl(float8_mul(l2.a, x)?, l2.c)?, l2.b)?;
    } else {
        return Ok(None);
    }
    // PostgreSQL changes -0 to 0.
    if x == 0.0 {
        x = 0.0;
    }
    if y == 0.0 {
        y = 0.0;
    }
    Ok(Some(Point { x, y }))
}

/// `line_contain_point`.
pub(crate) fn line_contain_point(l: Line, p: Point) -> Result<bool> {
    Ok(fp_zero(float8_pl(float8_pl(float8_mul(l.a, p.x)?, float8_mul(l.b, p.y)?)?, l.c)?))
}

/// `line_construct_pp`: the line through two points, which must not be equal.
fn line_construct_pp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (p, q) = (point(args, 0)?, point(args, 1)?);
    if point_eq(p, q) {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "invalid line specification: must be two distinct points",
        ));
    }
    Ok(line_value(line_construct(p, point_sl(p, q)?)?))
}

/// `line_perp`.
fn line_perp(l1: Line, l2: Line) -> Result<bool> {
    if fp_zero(l1.a) {
        return Ok(fp_zero(l2.b));
    }
    if fp_zero(l2.a) {
        return Ok(fp_zero(l1.b));
    }
    if fp_zero(l1.b) {
        return Ok(fp_zero(l2.a));
    }
    if fp_zero(l2.b) {
        return Ok(fp_zero(l1.a));
    }
    Ok(fp_eq(float8_div(float8_mul(l1.a, l2.a)?, float8_mul(l1.b, l2.b)?)?, -1.0))
}

/// `line_eq`: lines with proportional coefficients are equal, and a NaN coefficient needs the exact equality.
fn line_eq(l1: Line, l2: Line) -> Result<bool> {
    if [l1.a, l1.b, l1.c, l2.a, l2.b, l2.c].iter().any(|v| v.is_nan()) {
        return Ok(float8_eq(l1.a, l2.a) && float8_eq(l1.b, l2.b) && float8_eq(l1.c, l2.c));
    }
    let ratio = if !fp_zero(l2.a) {
        float8_div(l1.a, l2.a)?
    } else if !fp_zero(l2.b) {
        float8_div(l1.b, l2.b)?
    } else if !fp_zero(l2.c) {
        float8_div(l1.c, l2.c)?
    } else {
        1.0
    };
    Ok(fp_eq(l1.a, float8_mul(ratio, l2.a)?)
        && fp_eq(l1.b, float8_mul(ratio, l2.b)?)
        && fp_eq(l1.c, float8_mul(ratio, l2.c)?))
}

/// `line_distance`: 0 for lines that cross, else the distance between the parallel lines.
fn line_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (l1, l2) = (line(args, 0)?, line(args, 1)?);
    if line_interpt_line(l1, l2)?.is_some() {
        return Ok(Value::Float8(0.0));
    }
    let usable = |v: f64| !fp_zero(v) && !v.is_nan();
    let ratio = if usable(l1.a) && usable(l2.a) {
        float8_div(l1.a, l2.a)?
    } else if usable(l1.b) && usable(l2.b) {
        float8_div(l1.b, l2.b)?
    } else {
        1.0
    };
    let gap = float8_mi(l1.c, float8_mul(ratio, l2.c)?)?.abs();
    Ok(Value::Float8(float8_div(gap, l1.a.hypot(l1.b))?))
}

/// `lseg_sl`.
pub(crate) fn lseg_sl(s: Lseg) -> Result<f64> {
    point_sl(s.p[0], s.p[1])
}

/// The length of a segment.
fn lseg_len(s: Lseg) -> Result<f64> {
    point_dt(s.p[0], s.p[1])
}

/// `lseg_contain_point`: true when the point is on the segment. The sum of the distances uses no check of overflow, as in PostgreSQL.
pub(crate) fn lseg_contain_point(s: Lseg, p: Point) -> Result<bool> {
    let ends = point_dt(p, s.p[0])? + point_dt(p, s.p[1])?;
    Ok(fp_eq(ends, point_dt(s.p[0], s.p[1])?))
}

/// `lseg_interpt_line`: the point where a segment crosses a line. An end of the segment that is equal to the point is the result, which removes the rounding errors.
pub(crate) fn lseg_interpt_line(s: Lseg, l: Line) -> Result<Option<Point>> {
    let tmp = line_construct(s.p[0], lseg_sl(s)?)?;
    let Some(p) = line_interpt_line(tmp, l)? else { return Ok(None) };
    if !lseg_contain_point(s, p)? {
        return Ok(None);
    }
    Ok(Some(if point_eq(s.p[0], p) {
        s.p[0]
    } else if point_eq(s.p[1], p) {
        s.p[1]
    } else {
        p
    }))
}

/// `lseg_interpt_lseg`: the point where two segments cross.
pub(crate) fn lseg_interpt_lseg(s1: Lseg, s2: Lseg) -> Result<Option<Point>> {
    let tmp = line_construct(s2.p[0], lseg_sl(s2)?)?;
    let Some(p) = lseg_interpt_line(s1, tmp)? else { return Ok(None) };
    if !lseg_contain_point(s2, p)? {
        return Ok(None);
    }
    Ok(Some(p))
}

/// `line_closept_point`: the distance from a point to a line and the closest point of the line. The distance is NaN when the perpendicular line does not cross the line.
pub(crate) fn line_closept_point(l: Line, p: Point) -> Result<(f64, Point)> {
    let tmp = line_construct(p, line_invsl(l)?)?;
    match line_interpt_line(tmp, l)? {
        Some(c) => Ok((point_dt(c, p)?, c)),
        None => Ok((f64::NAN, p)),
    }
}

/// `lseg_closept_line`: the distance from a segment to a line and the closest point of the segment.
pub(crate) fn lseg_closept_line(s: Lseg, l: Line) -> Result<(f64, Point)> {
    if let Some(p) = lseg_interpt_line(s, l)? {
        return Ok((0.0, p));
    }
    let d1 = line_closept_point(l, s.p[0])?.0;
    let d2 = line_closept_point(l, s.p[1])?.0;
    Ok(if d1 < d2 { (d1, s.p[0]) } else { (d2, s.p[1]) })
}

/// `lseg_closept_point`: the distance from a point to a segment and the closest point of the segment.
pub(crate) fn lseg_closept_point(s: Lseg, p: Point) -> Result<(f64, Point)> {
    let tmp = line_construct(p, point_invsl(s.p[0], s.p[1])?)?;
    let c = lseg_closept_line(s, tmp)?.1;
    Ok((point_dt(c, p)?, c))
}

/// `lseg_closept_lseg`: the distance between two segments and the closest point of the first segment.
pub(crate) fn lseg_closept_lseg(on: Lseg, to: Lseg) -> Result<(f64, Point)> {
    if let Some(p) = lseg_interpt_lseg(on, to)? {
        return Ok((0.0, p));
    }
    let (mut dist, mut result) = lseg_closept_point(on, to.p[0])?;
    let (d, p) = lseg_closept_point(on, to.p[1])?;
    if float8_lt(d, dist) {
        (dist, result) = (d, p);
    }
    for end in on.p {
        let d = lseg_closept_point(to, end)?.0;
        if float8_lt(d, dist) {
            (dist, result) = (d, end);
        }
    }
    Ok((dist, result))
}

/// `box_construct`: the box with two corners.
pub(crate) fn box_construct(p: Point, q: Point) -> Rect {
    let (hx, lx) = if float8_gt(p.x, q.x) { (p.x, q.x) } else { (q.x, p.x) };
    let (hy, ly) = if float8_gt(p.y, q.y) { (p.y, q.y) } else { (q.y, p.y) };
    Rect { high: Point { x: hx, y: hy }, low: Point { x: lx, y: ly } }
}

/// `box_ov`: true when two boxes overlap.
pub(crate) fn box_ov(b: Rect, c: Rect) -> bool {
    fp_le(b.low.x, c.high.x)
        && fp_le(c.low.x, b.high.x)
        && fp_le(b.low.y, c.high.y)
        && fp_le(c.low.y, b.high.y)
}

/// `box_contain_box`.
fn box_contain_box(outer: Rect, inner: Rect) -> bool {
    fp_ge(outer.high.x, inner.high.x)
        && fp_le(outer.low.x, inner.low.x)
        && fp_ge(outer.high.y, inner.high.y)
        && fp_le(outer.low.y, inner.low.y)
}

/// `box_contain_point`, with the exact comparisons.
pub(crate) fn box_contain_point(b: Rect, p: Point) -> bool {
    b.high.x >= p.x && b.low.x <= p.x && b.high.y >= p.y && b.low.y <= p.y
}

/// `box_wd`.
pub(crate) fn box_wd(b: Rect) -> Result<f64> {
    float8_mi(b.high.x, b.low.x)
}

/// `box_ht`.
pub(crate) fn box_ht(b: Rect) -> Result<f64> {
    float8_mi(b.high.y, b.low.y)
}

/// `box_ar`.
pub(crate) fn box_ar(b: Rect) -> Result<f64> {
    float8_mul(box_wd(b)?, box_ht(b)?)
}

/// `box_cn`: the center of a box.
pub(crate) fn box_cn(b: Rect) -> Result<Point> {
    let x = float8_div(float8_pl(b.high.x, b.low.x)?, 2.0)?;
    let y = float8_div(float8_pl(b.high.y, b.low.y)?, 2.0)?;
    Ok(Point { x, y })
}

/// The four sides of a box in the order of `geo_ops.c`: from the low corner up, from the high corner left, from the low corner right, and from the high corner down.
fn box_sides(b: Rect) -> [Lseg; 4] {
    let top_left = Point { x: b.low.x, y: b.high.y };
    let bottom_right = Point { x: b.high.x, y: b.low.y };
    [
        Lseg { p: [b.low, top_left] },
        Lseg { p: [b.high, top_left] },
        Lseg { p: [b.low, bottom_right] },
        Lseg { p: [b.high, bottom_right] },
    ]
}

/// `box_closept_point`: the distance from a point to a box and the closest point of the box.
pub(crate) fn box_closept_point(b: Rect, p: Point) -> Result<(f64, Point)> {
    if box_contain_point(b, p) {
        return Ok((0.0, p));
    }
    let [first, rest @ ..] = box_sides(b);
    let (mut dist, mut result) = lseg_closept_point(first, p)?;
    for side in rest {
        let (d, c) = lseg_closept_point(side, p)?;
        if float8_lt(d, dist) {
            (dist, result) = (d, c);
        }
    }
    Ok((dist, result))
}

/// The box with the ends of a segment as corners, as `box_interpt_lseg` makes it.
fn lseg_bound_box(s: Lseg) -> Rect {
    Rect {
        high: Point { x: float8_max(s.p[0].x, s.p[1].x), y: float8_max(s.p[0].y, s.p[1].y) },
        low: Point { x: float8_min(s.p[0].x, s.p[1].x), y: float8_min(s.p[0].y, s.p[1].y) },
    }
}

/// The part of `box_interpt_lseg` after the check of the bounding boxes: true when an end of the segment is in the box or the segment crosses a side of the box.
fn box_touch_lseg(b: Rect, s: Lseg) -> Result<bool> {
    if box_contain_point(b, s.p[0]) || box_contain_point(b, s.p[1]) {
        return Ok(true);
    }
    for side in box_sides(b) {
        if lseg_interpt_lseg(side, s)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `box_interpt_lseg` with no result point: true when a segment and a box have a common point.
pub(crate) fn box_interpt_lseg(b: Rect, s: Lseg) -> Result<bool> {
    Ok(box_ov(lseg_bound_box(s), b) && box_touch_lseg(b, s)?)
}

/// `box_closept_lseg`: the distance from a segment to a box and the closest point of the box. For a segment that touches the box, the point is the point of the segment that is closest to the center of the box, as in PostgreSQL. PostgreSQL finds that point only when the caller wants the point, so `want` is false for a distance.
pub(crate) fn box_closept_lseg(b: Rect, s: Lseg, want: bool) -> Result<(f64, Point)> {
    if box_ov(lseg_bound_box(s), b) {
        let near = if want { lseg_closept_point(s, box_cn(b)?)?.1 } else { s.p[0] };
        if box_touch_lseg(b, s)? {
            return Ok((0.0, near));
        }
    }
    let [first, rest @ ..] = box_sides(b);
    let (mut dist, mut result) = lseg_closept_lseg(first, s)?;
    for side in rest {
        let (d, c) = lseg_closept_lseg(side, s)?;
        if float8_lt(d, dist) {
            (dist, result) = (d, c);
        }
    }
    Ok((dist, result))
}

/// `box_intersect`: the common part of two boxes, or NULL for boxes that do not overlap.
fn box_intersect(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (b, c) = (rect(args, 0)?, rect(args, 1)?);
    if !box_ov(b, c) {
        return Ok(Value::Null);
    }
    Ok(box_value(Rect {
        high: Point { x: float8_min(b.high.x, c.high.x), y: float8_min(b.high.y, c.high.y) },
        low: Point { x: float8_max(b.low.x, c.low.x), y: float8_max(b.low.y, c.low.y) },
    }))
}

/// `boxes_bound_box`: the smallest box that holds two boxes.
fn boxes_bound_box(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (b, c) = (rect(args, 0)?, rect(args, 1)?);
    Ok(box_value(Rect {
        high: Point { x: float8_max(b.high.x, c.high.x), y: float8_max(b.high.y, c.high.y) },
        low: Point { x: float8_min(b.low.x, c.low.x), y: float8_min(b.low.y, c.low.y) },
    }))
}

/// `box_add` and `box_sub`: each corner moves by the point and the corners keep their order.
fn box_translate(args: &[Value], op: fn(Point, Point) -> Result<Point>) -> Result<Value> {
    let (b, p) = (rect(args, 0)?, point(args, 1)?);
    Ok(box_value(Rect { high: op(b.high, p)?, low: op(b.low, p)? }))
}

/// `box_mul` and `box_div`: each corner changes as a complex number, then `box_construct` puts the corners in order.
fn box_scale(args: &[Value], op: fn(Point, Point) -> Result<Point>) -> Result<Value> {
    let (b, p) = (rect(args, 0)?, point(args, 1)?);
    let high = op(b.high, p)?;
    let low = op(b.low, p)?;
    Ok(box_value(box_construct(high, low)))
}

/// `close_lseg`: the point of the second segment that is closest to the first segment, or NULL for segments with the same slope.
fn close_lseg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (s1, s2) = (lseg(args, 0)?, lseg(args, 1)?);
    if lseg_sl(s1)? == lseg_sl(s2)? {
        return Ok(Value::Null);
    }
    Ok(closest(lseg_closept_lseg(s2, s1)?))
}

/// `close_ls`: the point of the segment that is closest to the line, or NULL for a segment with the slope of the line.
fn close_ls(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (l, s) = (line(args, 0)?, lseg(args, 1)?);
    if lseg_sl(s)? == line_sl(l)? {
        return Ok(Value::Null);
    }
    Ok(closest(lseg_closept_line(s, l)?))
}

/// `inter_lb`: true when a line crosses a side of a box.
fn inter_lb(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (l, b) = (line(args, 0)?, rect(args, 1)?);
    let corners = [
        b.low,
        Point { x: b.low.x, y: b.high.y },
        b.high,
        Point { x: b.high.x, y: b.low.y },
        b.low,
    ];
    for pair in corners.windows(2) {
        if lseg_interpt_line(Lseg { p: [pair[0], pair[1]] }, l)?.is_some() {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

/// A `path`: the points, and the flag of a closed path.
#[derive(Clone, Debug)]
struct Path {
    closed: bool,
    p: Vec<Point>,
}

/// A `circle`: the center and the radius.
#[derive(Clone, Copy, Debug)]
struct Circle {
    center: Point,
    radius: f64,
}

/// The points of the binary form of a `path` or a `polygon` after the flag: the number of points as an `int4`, then the points. There is at least one point.
fn point_list(bytes: &[u8]) -> Result<Vec<Point>> {
    let Some((npts, rest)) = bytes.split_first_chunk::<4>() else { return Err(bad_value()) };
    let (floats, tail) = rest.as_chunks::<8>();
    let (pairs, odd) = floats.as_chunks::<2>();
    let npts = usize::try_from(i32::from_be_bytes(*npts)).ok();
    if pairs.is_empty() || !tail.is_empty() || !odd.is_empty() || npts != Some(pairs.len()) {
        return Err(bad_value());
    }
    Ok(pairs
        .iter()
        .map(|[x, y]| Point { x: f64::from_be_bytes(*x), y: f64::from_be_bytes(*y) })
        .collect())
}

/// The `path` of argument `n`.
fn path(args: &[Value], n: usize) -> Result<Path> {
    let Some(Value::Bytea(bytes)) = args.get(n) else { return Err(bad_value()) };
    let Some((&closed, rest)) = bytes.split_first() else { return Err(bad_value()) };
    Ok(Path { closed: closed != 0, p: point_list(rest)? })
}

/// The points of the `polygon` of argument `n`.
fn polygon(args: &[Value], n: usize) -> Result<Vec<Point>> {
    let Some(Value::Bytea(bytes)) = args.get(n) else { return Err(bad_value()) };
    point_list(bytes)
}

/// The `circle` of argument `n`.
fn circle(args: &[Value], n: usize) -> Result<Circle> {
    let [x, y, radius] = floats(args, n)?;
    Ok(Circle { center: Point { x, y }, radius })
}

/// The number of points as an `int4`.
fn count(p: &[Point]) -> i32 {
    i32::try_from(p.len()).unwrap_or(i32::MAX)
}

/// Adds the number of points and the points to a binary form.
fn put_points(out: &mut Vec<u8>, p: &[Point]) {
    out.extend_from_slice(&count(p).to_be_bytes());
    for q in p {
        out.extend_from_slice(&q.x.to_be_bytes());
        out.extend_from_slice(&q.y.to_be_bytes());
    }
}

/// A `path` value.
fn path_value(closed: bool, p: &[Point]) -> Value {
    let mut out = vec![u8::from(closed)];
    put_points(&mut out, p);
    Value::Bytea(out)
}

/// A `polygon` value.
fn poly_value(p: &[Point]) -> Value {
    let mut out = Vec::new();
    put_points(&mut out, p);
    Value::Bytea(out)
}

/// A `circle` value.
fn circle_value(c: Circle) -> Value {
    floats_value(&[c.center.x, c.center.y, c.radius])
}

/// A test of the bounding boxes of two polygons.
fn polys(args: &[Value], test: fn(Rect, Rect) -> bool) -> Result<Value> {
    Ok(Value::Bool(test(bound_box(&polygon(args, 0)?), bound_box(&polygon(args, 1)?))))
}

/// A test of two circles.
fn circles(args: &[Value], test: fn(Circle, Circle) -> Result<bool>) -> Result<Value> {
    Ok(Value::Bool(test(circle(args, 0)?, circle(args, 1)?)?))
}

/// `FPne`.
fn fp_ne(a: f64, b: f64) -> bool {
    a != b && (a - b).abs() > EPSILON
}

/// `make_bound_box`: the smallest box that holds the points.
fn bound_box(p: &[Point]) -> Rect {
    let mut b = Rect { high: p[0], low: p[0] };
    for q in &p[1..] {
        if float8_lt(q.x, b.low.x) {
            b.low.x = q.x;
        }
        if float8_gt(q.x, b.high.x) {
            b.high.x = q.x;
        }
        if float8_lt(q.y, b.low.y) {
            b.low.y = q.y;
        }
        if float8_gt(q.y, b.high.y) {
            b.high.y = q.y;
        }
    }
    b
}

/// The segments of a path in the order of the loops of `geo_ops.c`: first the segment that closes a closed path, then the segment to each point after the first point.
fn path_segments(path: &Path) -> impl Iterator<Item = Lseg> + '_ {
    let closing = match (path.closed, path.p.last(), path.p.first()) {
        (true, Some(&last), Some(&first)) => Some(Lseg { p: [last, first] }),
        _ => None,
    };
    closing.into_iter().chain(path.p.windows(2).map(|w| Lseg { p: [w[0], w[1]] }))
}

/// `path_area`: the area of a closed path, or NULL for an open path.
fn path_area(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let path = path(args, 0)?;
    if !path.closed {
        return Ok(Value::Null);
    }
    let mut area = 0.0;
    for (p, q) in path.p.iter().zip(path.p.iter().cycle().skip(1)) {
        area = float8_pl(area, float8_mul(p.x, q.y)?)?;
        area = float8_mi(area, float8_mul(p.y, q.x)?)?;
    }
    Ok(Value::Float8(float8_div(area.abs(), 2.0)?))
}

/// `path_inter`: true when two paths cross. The bounding boxes give a quick check first.
fn path_inter(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (p1, p2) = (path(args, 0)?, path(args, 1)?);
    if !box_ov(bound_box(&p1.p), bound_box(&p2.p)) {
        return Ok(Value::Bool(false));
    }
    for s1 in path_segments(&p1) {
        for s2 in path_segments(&p2) {
            if lseg_interpt_lseg(s1, s2)?.is_some() {
                return Ok(Value::Bool(true));
            }
        }
    }
    Ok(Value::Bool(false))
}

/// The smallest distance between the segments of two paths, or `None` when a path has no segment.
fn segments_distance(p1: &Path, p2: &Path) -> Result<Option<f64>> {
    let mut min = None;
    for s1 in path_segments(p1) {
        for s2 in path_segments(p2) {
            let d = lseg_closept_lseg(s1, s2)?.0;
            if min.is_none_or(|m| float8_lt(d, m)) {
                min = Some(d);
            }
        }
    }
    Ok(min)
}

/// `path_distance`: the distance between two paths, or NULL when a path has no segment.
fn path_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let min = segments_distance(&path(args, 0)?, &path(args, 1)?)?;
    Ok(min.map_or(Value::Null, Value::Float8))
}

/// The translation, the turn or the scale of each point of a path.
fn path_translate(args: &[Value], op: fn(Point, Point) -> Result<Point>) -> Result<Value> {
    let (mut path, q) = (path(args, 0)?, point(args, 1)?);
    for p in &mut path.p {
        *p = op(*p, q)?;
    }
    Ok(path_value(path.closed, &path.p))
}

/// `dist_ppath_internal`: the distance from a point to a path. It is 0 for an open path with one point.
fn dist_ppath(pt: Point, path: &Path) -> Result<f64> {
    let mut result = None;
    for s in path_segments(path) {
        let d = lseg_closept_point(s, pt)?.0;
        if result.is_none_or(|r| float8_lt(d, r)) {
            result = Some(d);
        }
    }
    Ok(result.unwrap_or(0.0))
}

/// `on_ppath`: true when the point is on an open path, or in a closed path.
fn on_ppath(pt: Point, path: &Path) -> Result<bool> {
    if path.closed {
        return Ok(point_inside(pt, &path.p)? != 0);
    }
    let mut a = point_dt(pt, path.p[0])?;
    for w in path.p.windows(2) {
        let b = point_dt(pt, w[1])?;
        if fp_eq(float8_pl(a, b)?, point_dt(w[0], w[1])?) {
            return Ok(true);
        }
        a = b;
    }
    Ok(false)
}

/// The SQL function `path_contain_pt(path, point)`, which calls `on_ppath` with the arguments in the other order.
pub(crate) fn path_contain_pt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(on_ppath(point(args, 1)?, &path(args, 0)?)?))
}

/// `point_inside`: 1 when the point is in the polygon, 2 when it is on an edge, and 0 when it is outside. The sum of the crossings of the positive X axis is 0 for a point outside.
fn point_inside(p: Point, plist: &[Point]) -> Result<i32> {
    let x0 = float8_mi(plist[0].x, p.x)?;
    let y0 = float8_mi(plist[0].y, p.y)?;
    let (mut prev_x, mut prev_y) = (x0, y0);
    let mut total_cross = 0;
    for q in &plist[1..] {
        let x = float8_mi(q.x, p.x)?;
        let y = float8_mi(q.y, p.y)?;
        let Some(cross) = lseg_crossing(x, y, prev_x, prev_y)? else { return Ok(2) };
        total_cross += cross;
        (prev_x, prev_y) = (x, y);
    }
    let Some(cross) = lseg_crossing(x0, y0, prev_x, prev_y)? else { return Ok(2) };
    total_cross += cross;
    Ok(i32::from(total_cross != 0))
}

/// `lseg_crossing`: 2 or -2 when the segment crosses the positive X axis up or down, 1 or -1 when one end is on the positive X axis, and 0 for no crossing. `None` is `POINT_ON_POLYGON`: the segment holds the origin.
fn lseg_crossing(x: f64, y: f64, prev_x: f64, prev_y: f64) -> Result<Option<i32>> {
    if fp_zero(y) {
        if fp_zero(x) {
            return Ok(None);
        }
        if fp_gt(x, 0.0) {
            if fp_zero(prev_y) {
                return Ok(fp_gt(prev_x, 0.0).then_some(0));
            }
            return Ok(Some(if fp_lt(prev_y, 0.0) { 1 } else { -1 }));
        }
        if fp_zero(prev_y) {
            return Ok(fp_lt(prev_x, 0.0).then_some(0));
        }
        return Ok(Some(0));
    }
    let y_sign = if fp_gt(y, 0.0) { 1 } else { -1 };
    if fp_zero(prev_y) {
        return Ok(Some(if fp_lt(prev_x, 0.0) { 0 } else { y_sign }));
    }
    if (y_sign < 0 && fp_lt(prev_y, 0.0)) || (y_sign > 0 && fp_gt(prev_y, 0.0)) {
        return Ok(Some(0));
    }
    if fp_ge(x, 0.0) && fp_gt(prev_x, 0.0) {
        return Ok(Some(2 * y_sign));
    }
    if fp_lt(x, 0.0) && fp_le(prev_x, 0.0) {
        return Ok(Some(0));
    }
    let z =
        float8_mi(float8_mul(float8_mi(x, prev_x)?, y)?, float8_mul(float8_mi(y, prev_y)?, x)?)?;
    if fp_zero(z) {
        return Ok(None);
    }
    if (y_sign < 0 && fp_lt(z, 0.0)) || (y_sign > 0 && fp_gt(z, 0.0)) {
        return Ok(Some(0));
    }
    Ok(Some(2 * y_sign))
}

/// `plist_same`: true when two lists of the same length have the same points in the same cycle, forward or backward.
fn plist_same(p1: &[Point], p2: &[Point]) -> bool {
    let n = p1.len();
    for (i, &first) in p2.iter().enumerate() {
        if !point_eq(first, p1[0]) {
            continue;
        }
        if (1..n).all(|ii| point_eq(p2[(i + ii) % n], p1[ii])) {
            return true;
        }
        if (1..n).all(|ii| point_eq(p2[(i + n - ii) % n], p1[ii])) {
            return true;
        }
    }
    false
}

/// `poly_overlap_internal`: true when two polygons overlap. Two edges that cross give true. If no edges cross, a polygon that holds the first point of the other polygon gives true.
fn poly_overlap(pa: &[Point], pb: &[Point]) -> Result<bool> {
    if !box_ov(bound_box(pa), bound_box(pb)) {
        return Ok(false);
    }
    let mut a0 = pa[pa.len() - 1];
    for &a1 in pa {
        let mut b0 = pb[pb.len() - 1];
        for &b1 in pb {
            if lseg_interpt_lseg(Lseg { p: [a0, a1] }, Lseg { p: [b0, b1] })?.is_some() {
                return Ok(true);
            }
            b0 = b1;
        }
        a0 = a1;
    }
    Ok(point_inside(pa[0], pb)? != 0 || point_inside(pb[0], pa)? != 0)
}

/// `touched_lseg_inside_poly`: the test of the segment from `a` to `b`, where `a` is on the edge `s` and the segment is not in `s`.
fn touched_lseg_inside_poly(
    a: Point,
    b: Point,
    s: Lseg,
    poly: &[Point],
    start: usize,
) -> Result<bool> {
    let t = Lseg { p: [a, b] };
    if point_eq(a, s.p[0]) {
        if lseg_contain_point(t, s.p[1])? {
            return lseg_inside_poly(b, s.p[1], poly, start);
        }
    } else if point_eq(a, s.p[1]) {
        if lseg_contain_point(t, s.p[0])? {
            return lseg_inside_poly(b, s.p[0], poly, start);
        }
    } else if lseg_contain_point(t, s.p[0])? {
        return lseg_inside_poly(b, s.p[0], poly, start);
    } else if lseg_contain_point(t, s.p[1])? {
        return lseg_inside_poly(b, s.p[1], poly, start);
    }
    Ok(true)
}

/// `lseg_inside_poly`: true when the segment from `a` to `b` is in the polygon. The check starts at the edge that ends at point `start`. A segment that crosses an edge is cut in two parts, and each part gets the check again.
fn lseg_inside_poly(a: Point, b: Point, poly: &[Point], start: usize) -> Result<bool> {
    let t = Lseg { p: [a, b] };
    let mut res = true;
    let mut intersection = false;
    let mut s0 = poly[if start == 0 { poly.len() - 1 } else { start - 1 }];
    for (i, &s1) in poly.iter().enumerate().skip(start) {
        if !res {
            break;
        }
        let s = Lseg { p: [s0, s1] };
        if lseg_contain_point(s, t.p[0])? {
            if lseg_contain_point(s, t.p[1])? {
                return Ok(true);
            }
            res = touched_lseg_inside_poly(t.p[0], t.p[1], s, poly, i + 1)?;
        } else if lseg_contain_point(s, t.p[1])? {
            res = touched_lseg_inside_poly(t.p[1], t.p[0], s, poly, i + 1)?;
        } else if let Some(interpt) = lseg_interpt_lseg(t, s)? {
            intersection = true;
            res = lseg_inside_poly(t.p[0], interpt, poly, i + 1)?;
            if res {
                res = lseg_inside_poly(t.p[1], interpt, poly, i + 1)?;
            }
        }
        s0 = s1;
    }
    if res && !intersection {
        let x = float8_div(float8_pl(t.p[0].x, t.p[1].x)?, 2.0)?;
        let y = float8_div(float8_pl(t.p[0].y, t.p[1].y)?, 2.0)?;
        res = point_inside(Point { x, y }, poly)? != 0;
    }
    Ok(res)
}

/// `poly_contain_poly`: true when each edge of the second polygon is in the first polygon.
fn poly_contain_poly(outer: &[Point], inner: &[Point]) -> Result<bool> {
    if !box_contain_box(bound_box(outer), bound_box(inner)) {
        return Ok(false);
    }
    let mut s0 = inner[inner.len() - 1];
    for &s1 in inner {
        if !lseg_inside_poly(s0, s1, outer, 0)? {
            return Ok(false);
        }
        s0 = s1;
    }
    Ok(true)
}

/// `poly_distance`: 0 for two polygons that overlap, else the smallest distance between the edges.
fn poly_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (pa, pb) = (polygon(args, 0)?, polygon(args, 1)?);
    if poly_overlap(&pa, &pb)? {
        return Ok(Value::Float8(0.0));
    }
    let (pa, pb) = (Path { closed: true, p: pa }, Path { closed: true, p: pb });
    Ok(segments_distance(&pa, &pb)?.map_or(Value::Null, Value::Float8))
}

/// `dist_ppoly_internal`: 0 for a point in the polygon, else the smallest distance from the point to an edge.
fn dist_ppoly(pt: Point, poly: &[Point]) -> Result<f64> {
    if point_inside(pt, poly)? != 0 {
        return Ok(0.0);
    }
    let mut result = lseg_closept_point(Lseg { p: [poly[0], poly[poly.len() - 1]] }, pt)?.0;
    for w in poly.windows(2) {
        let d = lseg_closept_point(Lseg { p: [w[0], w[1]] }, pt)?.0;
        if float8_lt(d, result) {
            result = d;
        }
    }
    Ok(result)
}

/// `dist_cpoly_internal`: the distance from the center to the polygon, less the radius, and 0 when that is negative.
fn dist_cpoly(c: Circle, poly: &[Point]) -> Result<f64> {
    let result = float8_mi(dist_ppoly(c.center, poly)?, c.radius)?;
    Ok(if result < 0.0 { 0.0 } else { result })
}

/// `poly_to_circle`: the center is the mean of the points, and the radius is the mean distance from the center to the points.
fn poly_to_circle(poly: &[Point]) -> Result<Circle> {
    let mut center = Point { x: 0.0, y: 0.0 };
    for &p in poly {
        center = point_add(center, p)?;
    }
    let npts = f64::from(count(poly));
    center.x = float8_div(center.x, npts)?;
    center.y = float8_div(center.y, npts)?;
    let mut radius = 0.0;
    for &p in poly {
        radius = float8_pl(radius, point_dt(p, center)?)?;
    }
    Ok(Circle { center, radius: float8_div(radius, npts)? })
}

/// `circle_ar`: the area of a circle.
fn circle_ar(c: Circle) -> Result<f64> {
    float8_mul(float8_mul(c.radius, c.radius)?, std::f64::consts::PI)
}

/// True when the point is in the circle or on it, with an exact comparison.
fn circle_contain_point(c: Circle, p: Point) -> Result<bool> {
    Ok(point_dt(c.center, p)? <= c.radius)
}

/// `dist_pc`: the distance from a point to the center, less the radius, and 0 when that is negative.
fn dist_pc(p: Point, c: Circle) -> Result<f64> {
    let result = float8_mi(point_dt(p, c.center)?, c.radius)?;
    Ok(if result < 0.0 { 0.0 } else { result })
}

/// `circle_poly_internal`: the polygon with `npts` points on the circle. The size checks give the errors of PostgreSQL for the size of the polygon in memory, which has 40 bytes of header and 16 bytes for each point.
fn circle_poly(npts: i32, c: Circle) -> Result<Value> {
    if fp_zero(c.radius) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot convert circle with radius zero to polygon",
        ));
    }
    if npts < 2 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "must request at least 2 points",
        ));
    }
    let size = 40 + 16 * i64::from(npts);
    if size > i64::from(i32::MAX) {
        return Err(Error::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "too many points requested"));
    }
    if size > 0x3fff_ffff {
        return Err(Error::new(
            SqlState::INTERNAL_ERROR,
            format!("invalid memory alloc request size {size}"),
        ));
    }
    let anglestep = float8_div(2.0 * std::f64::consts::PI, f64::from(npts))?;
    let mut poly = Vec::new();
    for i in 0..npts {
        let angle = float8_mul(anglestep, f64::from(i))?;
        let x = float8_mi(c.center.x, float8_mul(c.radius, angle.cos())?)?;
        let y = float8_pl(c.center.y, float8_mul(c.radius, angle.sin())?)?;
        poly.push(Point { x, y });
    }
    Ok(poly_value(&poly))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// The results of PostgreSQL 19 for the same values.
    #[test]
    fn points_and_lines() {
        let q = point_div(p(1.0, 2.0), p(3.0, 4.0)).unwrap();
        assert_eq!((q.x, q.y), (0.44, 0.08));
        assert!(point_div(p(1.0, 2.0), p(0.0, 0.0)).is_err());
        let l = line_construct(p(1.0, 2.0), point_sl(p(1.0, 2.0), p(3.0, 7.0)).unwrap()).unwrap();
        assert_eq!((l.a, l.b, l.c), (2.5, -1.0, -0.5));
        let cross =
            line_interpt_line(Line { a: 1.0, b: -1.0, c: 0.0 }, Line { a: 1.0, b: 1.0, c: -2.0 });
        let cross = cross.unwrap().unwrap();
        assert_eq!((cross.x, cross.y), (1.0, 1.0));
        let (d, c) =
            box_closept_point(Rect { high: p(2.0, 2.0), low: p(0.0, 0.0) }, p(5.0, 5.0)).unwrap();
        assert_eq!((d.to_bits(), c.x, c.y), (18f64.sqrt().to_bits(), 2.0, 2.0));
    }

    /// The results of PostgreSQL 19 for the same polygons and circles.
    #[test]
    fn polygons_and_circles() {
        let square = [p(0.0, 0.0), p(4.0, 0.0), p(4.0, 4.0), p(0.0, 4.0)];
        assert_eq!(point_inside(p(2.0, 2.0), &square).unwrap(), 1);
        assert_eq!(point_inside(p(4.0, 2.0), &square).unwrap(), 2);
        assert_eq!(point_inside(p(5.0, 2.0), &square).unwrap(), 0);
        let triangle = [p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0)];
        assert!(plist_same(&triangle, &[p(1.0, 1.0), p(1.0, 0.0), p(0.0, 0.0)]));
        assert!(!plist_same(&triangle, &[p(0.0, 0.0), p(1.0, 1.0), p(1.0, 2.0)]));
        assert!(poly_contain_poly(&square, &[p(1.0, 1.0), p(2.0, 1.0), p(2.0, 2.0)]).unwrap());
        assert!(!poly_contain_poly(&square, &[p(1.0, 1.0), p(5.0, 1.0), p(2.0, 2.0)]).unwrap());
        let c = poly_to_circle(&[p(0.0, 0.0), p(2.0, 0.0), p(2.0, 2.0), p(0.0, 2.0)]).unwrap();
        assert_eq!((c.center.x, c.center.y, c.radius), (1.0, 1.0, 2f64.sqrt()));
        assert!(circle_poly(1, c).is_err());
    }
}
