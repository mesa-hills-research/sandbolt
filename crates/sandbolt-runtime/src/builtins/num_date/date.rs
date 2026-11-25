mod parse;
mod zone;

use std::fmt::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::atom::Atom;
use crate::object::DateData;
use crate::ops::call::{call, is_callable};
use crate::ops::convert::to_object;
use crate::ops::property::get_property;

use super::*;

pub(super) const START: u16 = 0x100;
const CTOR: u16 = START;
const NOW: u16 = START + 1;
const PARSE: u16 = START + 2;
const UTC: u16 = START + 3;
const TO_PRIMITIVE: u16 = START + 4;
const TO_JSON: u16 = START + 5;
const GET_TIME: u16 = START + 6;
const VALUE_OF: u16 = START + 7;
const GET_OFFSET: u16 = START + 8;
const SET_TIME: u16 = START + 9;
const GET_YEAR: u16 = START + 10;
const SET_YEAR: u16 = START + 11;
const FORMAT: u16 = START + 12;
const GET: u16 = FORMAT + FORMATS.len() as u16;
const SET: u16 = GET + 2 * GETTERS.len() as u16;
pub(super) const END: u16 = SET + 2 * SETTERS.len() as u16 - 1;

const DAY_MS: i64 = 86_400_000;
const MIN_MS: i64 = 60_000;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

const GETTERS: &[(&str, &str)] = &[
    ("getFullYear", "getUTCFullYear"),
    ("getMonth", "getUTCMonth"),
    ("getDate", "getUTCDate"),
    ("getHours", "getUTCHours"),
    ("getMinutes", "getUTCMinutes"),
    ("getSeconds", "getUTCSeconds"),
    ("getMilliseconds", "getUTCMilliseconds"),
    ("getDay", "getUTCDay"),
];
const SETTERS: &[(&str, &str, u32)] = &[
    ("setFullYear", "setUTCFullYear", 3),
    ("setMonth", "setUTCMonth", 2),
    ("setDate", "setUTCDate", 1),
    ("setHours", "setUTCHours", 4),
    ("setMinutes", "setUTCMinutes", 3),
    ("setSeconds", "setUTCSeconds", 2),
    ("setMilliseconds", "setUTCMilliseconds", 1),
];

#[derive(Clone, Copy)]
enum Format {
    Utc,
    Iso,
    Local,
    Locale,
}

const FORMATS: &[(&str, Format, bool, bool)] = &[
    ("toUTCString", Format::Utc, true, true),
    ("toISOString", Format::Iso, true, true),
    ("toString", Format::Local, true, true),
    ("toDateString", Format::Local, true, false),
    ("toTimeString", Format::Local, false, true),
    ("toLocaleString", Format::Locale, true, true),
    ("toLocaleDateString", Format::Locale, true, false),
    ("toLocaleTimeString", Format::Locale, false, true),
];

const fn id(n: u16) -> NativeFnId {
    NativeFnId(NUM_DATE_BASE + n)
}

pub(super) fn install(ctx: &mut ExecCtx<'_, '_>) -> JsResult<()> {
    zone::init();
    let global = ctx.root(Value::object(ctx.realm().global_object))?;
    let ctor = constructor(ctx, global, ctx.realm().date_ctor, id(CTOR), "Date", 7)?;
    install_methods(
        ctx,
        ctor,
        &[
            ("now", 0, id(NOW)),
            ("parse", 1, id(PARSE)),
            ("UTC", 7, id(UTC)),
        ],
    )?;
    let proto = ctx.root(Value::object(ctx.realm().date_proto))?;
    install_methods(
        ctx,
        proto,
        &[
            ("toJSON", 1, id(TO_JSON)),
            ("valueOf", 0, id(VALUE_OF)),
            ("getTime", 0, id(GET_TIME)),
            ("getTimezoneOffset", 0, id(GET_OFFSET)),
            ("setTime", 1, id(SET_TIME)),
            ("getYear", 0, id(GET_YEAR)),
            ("setYear", 1, id(SET_YEAR)),
        ],
    )?;
    let key = symbol_key(ctx, WellKnown::ToPrimitive);
    crate::builtins::install_method_with_flags(
        ctx,
        proto,
        key,
        id(TO_PRIMITIVE),
        "[Symbol.toPrimitive]",
        1,
        PropFlags::CONFIGURABLE,
    )?;
    for (i, &(name, _, _, _)) in FORMATS.iter().enumerate() {
        if i == 0 {
            let function = make_native_fn(ctx, id(FORMAT), name, 0, false)?;
            define(ctx, proto, name, function, mutable())?;
            define(ctx, proto, "toGMTString", function, mutable())?;
        } else {
            let key = ctx.intern_str(name)?;
            crate::builtins::install_method(ctx, proto, key, id(FORMAT + i as u16), name, 0)?;
        }
    }
    for (i, &(local, utc)) in GETTERS.iter().enumerate() {
        let id0 = GET + 2 * i as u16;
        install_methods(ctx, proto, &[(local, 0, id(id0)), (utc, 0, id(id0 + 1))])?;
    }
    for (i, &(local, utc, count)) in SETTERS.iter().enumerate() {
        let id0 = SET + 2 * i as u16;
        install_methods(
            ctx,
            proto,
            &[(local, count, id(id0)), (utc, count, id(id0 + 1))],
        )?;
    }
    Ok(())
}

pub(super) fn dispatch<'s>(
    offset: u16,
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    match offset {
        CTOR => construct(ctx, args),
        NOW => Ok(Value::number(now())),
        PARSE => {
            let arg = argument(ctx, args, 0)?;
            let string = to_string(ctx, arg)?;
            let time = parse_string(ctx, string_id(ctx.get(string)))?;
            Ok(Value::number(time))
        }
        UTC => Ok(Value::number(if args.is_empty() {
            f64::NAN
        } else {
            clip(from_parts(ctx, args, false)?)
        })),
        TO_PRIMITIVE => to_primitive_method(ctx, this, args),
        TO_JSON => to_json(ctx, this),
        GET_TIME | VALUE_OF => Ok(Value::number(this_time(ctx, this)?)),
        GET_YEAR => {
            let t = this_time(ctx, this)?;
            Ok(Value::number(if t.is_nan() {
                t
            } else {
                fields(t as i64, true).parts[0] as f64 - 1900.0
            }))
        }
        GET_OFFSET => {
            let t = this_time(ctx, this)?;
            Ok(Value::number(if t.is_nan() {
                t
            } else {
                f64::from(-zone::offset(t as i64))
            }))
        }
        SET_TIME => {
            this_time(ctx, this)?;
            let arg = argument(ctx, args, 0)?;
            let time = clip(to_number(ctx, arg)?);
            Ok(set_time(ctx, this, time))
        }
        SET_YEAR => set_fields(ctx, this, args, 0, 1, true, true),
        FORMAT..GET => {
            let (_, format, date, time) = FORMATS[(offset - FORMAT) as usize];
            let t = this_time(ctx, this)?;
            if t.is_nan() && matches!(format, Format::Iso) {
                return Err(ctx.throw_range_error("invalid Date"));
            }
            format_string(ctx, t, format, date, time)
        }
        GET..SET => {
            let t = this_time(ctx, this)?;
            if t.is_nan() {
                return Ok(Value::number(t));
            }
            let n = offset - GET;
            let fields = fields(t as i64, n.is_multiple_of(2));
            let n = (n / 2) as usize;
            Ok(Value::number(if n < 7 {
                fields.parts[n] as f64
            } else {
                fields.weekday as f64
            }))
        }
        SET..=END => {
            let n = offset - SET;
            let field = (n / 2) as usize;
            set_fields(
                ctx,
                this,
                args,
                field,
                SETTERS[field].2 as usize,
                n.is_multiple_of(2),
                false,
            )
        }
        _ => unreachable!(),
    }
}

fn now() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        Err(error) => -(error.duration().as_millis() as f64),
    }
}

fn parse_string(ctx: &mut ExecCtx<'_, '_>, string: StrId) -> JsResult<f64> {
    let bytes = ascii_text(ctx, string, true)?;
    Ok(parse::parse(text_ref(&bytes)))
}

fn construct<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>) -> JsResult<Value> {
    let Some(target) = ctx.new_target() else {
        return format_string(ctx, now(), Format::Local, true, true);
    };
    let t = match args.len() {
        0 => now(),
        1 => {
            let arg = args.get(0).unwrap();
            let raw = ctx.get(arg);
            if let Some(id) = raw.as_object()
                && let ObjectData::Date(data) = &ctx.heap().object(id).data
            {
                data.time.as_number().unwrap()
            } else {
                let primitive = to_primitive(ctx, arg, PreferredType::Default)?;
                let rooted = ctx.root(primitive)?;
                if let ValueKind::String(string) = primitive.decode() {
                    parse_string(ctx, string)?
                } else {
                    to_number(ctx, rooted)?
                }
            }
        }
        _ => from_parts(ctx, args, true)?,
    };
    let time = Value::number(clip(t));
    let object = ctx.ordinary_create_from_constructor(
        target,
        |realm| realm.date_proto,
        ObjectFlags::EXTENSIBLE,
        |heap| {
            Ok(ObjectData::Date(TrackedBox::new(
                &heap.ledger,
                DateData { time },
            )?))
        },
    )?;
    Ok(ctx.get(object))
}

fn this_time<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<f64> {
    if let Some(id) = ctx.get(this).as_object()
        && let ObjectData::Date(data) = &ctx.heap().object(id).data
    {
        Ok(data.time.as_number().unwrap())
    } else {
        Err(ctx.throw_type_error("not a Date object"))
    }
}

fn set_time<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>, time: f64) -> Value {
    let id = ctx.get(this).as_object().unwrap();
    let ObjectData::Date(data) = &mut ctx.runtime.heap.objects.get_mut(id).unwrap().data else {
        unreachable!()
    };
    data.time = Value::number(time);
    data.time
}

fn clip(time: f64) -> f64 {
    if time.abs() <= 8.64e15 {
        time.trunc() + 0.0
    } else {
        f64::NAN
    }
}

fn days_from_year(year: i64) -> i64 {
    365 * (year - 1970) + (year - 1969).div_euclid(4) - (year - 1901).div_euclid(100)
        + (year - 1601).div_euclid(400)
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn month_days(year: i64, month: usize) -> i64 {
    MONTH_DAYS[month] + i64::from(month == 1 && leap(year))
}

fn year_from_days(days: i64) -> i64 {
    let mut year = 1970 + days * 10_000 / 3_652_425;
    while days < days_from_year(year) {
        year -= 1;
    }
    while days >= days_from_year(year + 1) {
        year += 1;
    }
    year
}

struct Fields {
    parts: [i64; 7],
    weekday: usize,
    offset: i32,
}

fn fields(time: i64, local: bool) -> Fields {
    let offset = if local { zone::offset(time) } else { 0 };
    let t = time + i64::from(offset) * MIN_MS;
    let days = t.div_euclid(DAY_MS);
    let time = t.rem_euclid(DAY_MS);
    let year = year_from_days(days);
    let mut day = days - days_from_year(year);
    let mut month = 0;
    while day >= month_days(year, month) {
        day -= month_days(year, month);
        month += 1;
    }
    Fields {
        parts: [
            year,
            month as i64,
            day + 1,
            time / 3_600_000,
            time / MIN_MS % 60,
            time / 1000 % 60,
            time % 1000,
        ],
        weekday: (days + 4).rem_euclid(7) as usize,
        offset,
    }
}

fn make_time(fields: [f64; 7], local: bool) -> f64 {
    let [y, m, d, h, min, s, ms] = fields.map(f64::trunc);
    if !fields.iter().all(|value| value.is_finite()) {
        return f64::NAN;
    }
    let year = y + (m / 12.0).floor();
    let month = m.rem_euclid(12.0) as usize;
    // An out-of-range calendar year can still produce an in-range timestamp
    // when the day or time overflows. Bound the integer calendar calculation.
    let max_year = (i64::MAX / 366 - 1970) as f64;
    if !(-max_year..=max_year).contains(&year) {
        return f64::NAN;
    }
    let year = year as i64;
    let days = days_from_year(year) + (0..month).map(|m| month_days(year, m)).sum::<i64>();
    let day = days as f64 + d - 1.0;
    let time = ((h * 3_600_000.0 + min * MIN_MS as f64) + s * 1000.0) + ms;
    let t = day * DAY_MS as f64 + time;
    if local && t.is_finite() && t.abs() <= 8.65e15 {
        t - f64::from(zone::local_offset(t as i64)) * MIN_MS as f64
    } else {
        t
    }
}

fn legacy_year(year: f64) -> f64 {
    let value = year.trunc();
    if (0.0..100.0).contains(&value) {
        value + 1900.0
    } else {
        year
    }
}

fn from_parts<'s>(ctx: &mut ExecCtx<'_, 's>, args: Args<'s>, local: bool) -> JsResult<f64> {
    let mut fields = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
    for (i, field) in fields.iter_mut().enumerate().take(args.len()) {
        *field = to_number(ctx, args.get(i).unwrap())?;
    }
    fields[0] = legacy_year(fields[0]);
    Ok(make_time(fields, local))
}

fn set_fields<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
    start: usize,
    count: usize,
    local: bool,
    year: bool,
) -> JsResult<Value> {
    let current = this_time(ctx, this)?;
    let mut next = if current.is_nan() {
        fields(0, false)
    } else {
        fields(current as i64, local)
    }
    .parts
    .map(|x| x as f64);
    for i in 0..args.len().clamp(1, count) {
        let arg = argument(ctx, args, i)?;
        next[start + i] = to_number(ctx, arg)?;
    }
    if current.is_nan() && start != 0 {
        return Ok(Value::number(f64::NAN));
    }
    if year {
        next[0] = legacy_year(next[0]);
    }
    let time = clip(make_time(next, local));
    Ok(set_time(ctx, this, time))
}

fn format_string<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    t: f64,
    format: Format,
    date: bool,
    time: bool,
) -> JsResult<Value> {
    let result = if t.is_nan() {
        ctx.new_string("Invalid Date")?
    } else {
        let local = matches!(format, Format::Local | Format::Locale);
        let fields = fields(t as i64, local);
        let [y, m, d, h, min, s, ms] = fields.parts;
        let weekday = WEEKDAYS[fields.weekday];
        let month = MONTHS[m as usize];
        let y_width = 4 + usize::from(y < 0);
        let mut text = DateText {
            bytes: [0; 64],
            len: 0,
        };
        if date {
            match format {
                Format::Utc => write!(text, "{weekday}, {d:02} {month} {y:0y_width$}"),
                Format::Iso => {
                    if (0..=9999).contains(&y) {
                        write!(text, "{y:04}")
                    } else {
                        write!(text, "{y:+07}")
                    }
                    .unwrap();
                    write!(text, "-{:02}-{d:02}", m + 1)
                }
                Format::Local => write!(text, "{weekday} {month} {d:02} {y:0y_width$}"),
                Format::Locale => write!(text, "{:02}/{d:02}/{y:0y_width$}", m + 1),
            }
            .unwrap();
        }
        if date && time {
            text.write_str(match format {
                Format::Iso => "T",
                Format::Locale => ", ",
                _ => " ",
            })
            .unwrap();
        }
        if time {
            match format {
                Format::Utc => write!(text, "{h:02}:{min:02}:{s:02} GMT"),
                Format::Iso => write!(text, "{h:02}:{min:02}:{s:02}.{ms:03}Z"),
                Format::Local => {
                    let sign = if fields.offset < 0 { '-' } else { '+' };
                    let offset = fields.offset.abs();
                    write!(
                        text,
                        "{h:02}:{min:02}:{s:02} GMT{sign}{:02}{:02}",
                        offset / 60,
                        offset % 60
                    )
                }
                Format::Locale => write!(
                    text,
                    "{:02}:{min:02}:{s:02} {}M",
                    (h + 11) % 12 + 1,
                    if h < 12 { 'A' } else { 'P' }
                ),
            }
            .unwrap();
        }
        ctx.new_string(text_ref(&text.bytes[..text.len]))?
    };
    Ok(ctx.get(result))
}

// The longest layout is a local date and time with a signed extended year.
struct DateText {
    bytes: [u8; 64],
    len: usize,
}

impl Write for DateText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn to_json<'s>(ctx: &mut ExecCtx<'_, 's>, this: Handle<'s>) -> JsResult<Value> {
    let object = to_object(ctx, this)?;
    let value = to_primitive(ctx, object, PreferredType::Number)?;
    if value.as_number().is_some_and(|n| !n.is_finite()) {
        return Ok(Value::NULL);
    }
    let key = ctx.intern_str("toISOString")?;
    let method = get_property(ctx, object, key)?;
    let method = ctx.root(method)?;
    call(ctx, method, object, Args::empty())
}

fn to_primitive_method<'s>(
    ctx: &mut ExecCtx<'_, 's>,
    this: Handle<'s>,
    args: Args<'s>,
) -> JsResult<Value> {
    ctx.object_id(this)?;
    let arg = argument(ctx, args, 0)?;
    let hint = if let ValueKind::String(id) = ctx.get(arg).decode() {
        id
    } else {
        return Err(ctx.throw_type_error("invalid Date hint"));
    };
    let matches = |text: &str| {
        ctx.heap()
            .string_units(hint)
            .eq(text.bytes().map(u16::from))
    };
    let names = if matches("number") {
        [Atom::VALUE_OF, Atom::TO_STRING]
    } else if matches("string") || matches("default") {
        [Atom::TO_STRING, Atom::VALUE_OF]
    } else {
        return Err(ctx.throw_type_error("invalid Date hint"));
    };
    for atom in names {
        let method = get_property(ctx, this, PropKey::atom(atom))?;
        if method
            .as_object()
            .is_some_and(|id| is_callable(ctx.heap(), id))
        {
            let method = ctx.root(method)?;
            let value = call(ctx, method, this, Args::empty())?;
            if value.as_object().is_none() {
                return Ok(value);
            }
        }
    }
    Err(ctx.throw_type_error("cannot convert Date to primitive"))
}
