use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{MIN_MS, days_from_year, leap, month_days, year_from_days};

const DAY: i64 = 86_400;

thread_local! {
    static LOCAL_ZONE: Zone = Zone::load();
}

pub(super) fn init() {
    LOCAL_ZONE.with(|_| {});
}

pub(super) fn offset(utc_ms: i64) -> i32 {
    LOCAL_ZONE.with(|zone| zone.offset(utc_ms.div_euclid(1_000)) / 60)
}

pub(super) fn local_offset(wall_ms: i64) -> i32 {
    LOCAL_ZONE.with(|zone| zone.wall_offset(wall_ms))
}

#[derive(Default)]
struct Zone {
    transitions: Vec<(i64, i32)>,
    initial: i32,
    future: Option<Rule>,
}

impl Zone {
    fn load() -> Self {
        let Some(tz) = std::env::var_os("TZ") else {
            return Self::read(Path::new("/etc/localtime")).unwrap_or_default();
        };
        let Some(tz) = tz.to_str() else {
            return Self::read(Path::new(&tz)).unwrap_or_default();
        };
        let tz = tz.strip_prefix(':').unwrap_or(tz);
        if tz.is_empty() {
            return Self::default();
        }
        let base = std::env::var_os("TZDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/usr/share/zoneinfo".into());
        Self::read(&base.join(tz))
            .or_else(|| {
                Rule::parse(tz).map(|rule| Self {
                    future: Some(rule),
                    ..Self::default()
                })
            })
            .unwrap_or_default()
    }

    fn read(path: &Path) -> Option<Self> {
        if !path.metadata().ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(1 << 20)
            .read_to_end(&mut bytes)
            .ok()?;
        Self::tzif(&bytes)
    }

    fn tzif(bytes: &[u8]) -> Option<Self> {
        let mut bytes = bytes;
        let (version, counts) = header(bytes)?;
        let width = if version == b'\0' {
            4
        } else {
            bytes = bytes.get(44 + counts.block_size(4)?..)?;
            8
        };
        let (_, counts) = header(bytes)?;
        bytes = bytes.get(44..)?;
        let tail = bytes.get(counts.block_size(width)?..)?;
        let n = counts.times;
        let times = bytes.get(..n.checked_mul(width)?)?;
        let indices = bytes.get(times.len()..times.len().checked_add(n)?)?;
        let types = bytes
            .get(times.len() + n..)?
            .get(..counts.types.checked_mul(6)?)?;
        let type_offset = |i: usize| {
            if i >= counts.types {
                return None;
            }
            Some(i32::from_be_bytes(
                types.get(i * 6..i * 6 + 4)?.try_into().ok()?,
            ))
        };
        let initial = type_offset(0)?;
        let mut transitions = Vec::with_capacity(n);
        for (chunk, index) in times.chunks_exact(width).zip(indices) {
            let at = if width == 8 {
                i64::from_be_bytes(chunk.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(chunk.try_into().ok()?))
            };
            transitions.push((at, type_offset(usize::from(*index))?));
        }
        let future = if width == 8 {
            std::str::from_utf8(tail)
                .ok()
                .and_then(|s| Rule::parse(s.trim_matches('\n')))
        } else {
            None
        };
        Some(Self {
            transitions,
            initial,
            future,
        })
    }

    fn offset(&self, utc: i64) -> i32 {
        if let Some(rule) = &self.future
            && self.transitions.last().is_none_or(|&(at, _)| utc > at)
        {
            return rule.offset(utc);
        }
        let i = self.transitions.partition_point(|&(at, _)| at <= utc);
        if i == 0 {
            self.initial
        } else {
            self.transitions[i - 1].1
        }
    }

    fn wall_offset(&self, wall_ms: i64) -> i32 {
        let wall = wall_ms.div_euclid(1_000);
        // Check both sides of a transition. The earlier instant wins in an
        // overlap. A skipped wall time uses the offset before the jump.
        let before = self.offset(wall - 2 * DAY) / 60;
        let after = self.offset(wall + 2 * DAY) / 60;
        for offset in [before.max(after), before.min(after)] {
            let utc = (wall_ms - i64::from(offset) * MIN_MS).div_euclid(1_000);
            if self.offset(utc) / 60 == offset {
                return offset;
            }
        }
        before
    }
}

struct Counts {
    ut: usize,
    standard: usize,
    leap: usize,
    times: usize,
    types: usize,
    chars: usize,
}

impl Counts {
    fn block_size(&self, width: usize) -> Option<usize> {
        self.times
            .checked_mul(width + 1)?
            .checked_add(self.types.checked_mul(6)?)?
            .checked_add(self.chars)?
            .checked_add(self.leap.checked_mul(width + 4)?)?
            .checked_add(self.standard)?
            .checked_add(self.ut)
    }
}

fn header(bytes: &[u8]) -> Option<(u8, Counts)> {
    let bytes = bytes.get(..44)?;
    if &bytes[..4] != b"TZif" || !matches!(bytes[4], b'\0' | b'2' | b'3' | b'4') {
        return None;
    }
    let mut counts = bytes[20..]
        .chunks_exact(4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()) as usize);
    Some((
        bytes[4],
        Counts {
            ut: counts.next()?,
            standard: counts.next()?,
            leap: counts.next()?,
            times: counts.next()?,
            types: counts.next()?,
            chars: counts.next()?,
        },
    ))
}

struct Rule {
    standard: i32,
    daylight: Option<Dst>,
}
struct Dst {
    offset: i32,
    start: Transition,
    end: Transition,
}
struct Transition {
    day: RuleDay,
    seconds: i32,
    basis: Basis,
}
enum Basis {
    Wall,
    Standard,
    Utc,
}
enum RuleDay {
    JulianWithoutLeap(i64),
    Julian(i64),
    Weekday {
        month: usize,
        week: i64,
        weekday: i64,
    },
}

impl Rule {
    fn parse(rule: &str) -> Option<Self> {
        let mut s = RuleText(rule.as_bytes());
        s.name()?;
        let standard = -s.hms(24)?;
        if s.0.is_empty() {
            return Some(Self {
                standard,
                daylight: None,
            });
        }
        s.name()?;
        let offset = if s.0.is_empty() || s.0.starts_with(b",") {
            standard + 3600
        } else {
            -s.hms(24)?
        };
        if s.0.is_empty() {
            s = RuleText(b",M3.2.0,M11.1.0");
        }
        s.eat(b',')?;
        let start = s.transition()?;
        s.eat(b',')?;
        let end = s.transition()?;
        s.0.is_empty().then_some(Self {
            standard,
            daylight: Some(Dst { offset, start, end }),
        })
    }

    fn offset(&self, utc: i64) -> i32 {
        let Some(dst) = &self.daylight else {
            return self.standard;
        };
        let year = year_from_days(utc.div_euclid(DAY));
        let mut result = (i64::MIN, self.standard);
        for y in year - 1..=year + 1 {
            for (t, offset) in [
                (dst.start.utc(y, self.standard, self.standard), dst.offset),
                (dst.end.utc(y, self.standard, dst.offset), self.standard),
            ] {
                if t <= utc && t >= result.0 {
                    result = (t, offset);
                }
            }
        }
        result.1
    }
}

impl Transition {
    fn utc(&self, year: i64, standard: i32, prior: i32) -> i64 {
        let year_start = days_from_year(year);
        let day = match self.day {
            RuleDay::JulianWithoutLeap(n) => year_start + n - 1 + i64::from(n >= 60 && leap(year)),
            RuleDay::Julian(n) => year_start + n,
            RuleDay::Weekday {
                month,
                week,
                weekday,
            } => {
                let start = year_start + (0..month).map(|m| month_days(year, m)).sum::<i64>();
                let mut day = (weekday - (start + 4)).rem_euclid(7) + (week - 1) * 7;
                if day >= month_days(year, month) {
                    day -= 7;
                }
                start + day
            }
        };
        let offset = match self.basis {
            Basis::Wall => prior,
            Basis::Standard => standard,
            Basis::Utc => 0,
        };
        day * DAY + i64::from(self.seconds) - i64::from(offset)
    }
}

struct RuleText<'a>(&'a [u8]);

impl RuleText<'_> {
    fn eat(&mut self, x: u8) -> Option<()> {
        self.0 = self.0.strip_prefix(&[x])?;
        Some(())
    }
    fn name(&mut self) -> Option<()> {
        if self.eat(b'<').is_some() {
            let end = self.0.iter().position(|&c| c == b'>')?;
            self.0 = &self.0[end + 1..];
            (end >= 3).then_some(())
        } else {
            let end = self
                .0
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count();
            self.0 = &self.0[end..];
            (end >= 3).then_some(())
        }
    }
    fn number(&mut self, min: i32, max: i32) -> Option<i32> {
        let mut num = 0_i32;
        let mut read = false;
        while let Some(&x) = self.0.first().filter(|x| x.is_ascii_digit()) {
            num = num.checked_mul(10)?.checked_add(i32::from(x - b'0'))?;
            self.0 = &self.0[1..];
            read = true;
        }
        (read && (min..=max).contains(&num)).then_some(num)
    }
    fn hms(&mut self, hour_max: i32) -> Option<i32> {
        let sign = if self.eat(b'-').is_some() {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut t = 3600 * self.number(0, hour_max)?;
        if self.eat(b':').is_some() {
            t += 60 * self.number(0, 59)?;
            if self.eat(b':').is_some() {
                t += self.number(0, 59)?;
            }
        }
        Some(sign * t)
    }
    fn transition(&mut self) -> Option<Transition> {
        let day = if self.eat(b'M').is_some() {
            let month = self.number(1, 12)? as usize - 1;
            self.eat(b'.')?;
            let week = i64::from(self.number(1, 5)?);
            self.eat(b'.')?;
            let weekday = i64::from(self.number(0, 6)?);
            RuleDay::Weekday {
                month,
                week,
                weekday,
            }
        } else if self.eat(b'J').is_some() {
            RuleDay::JulianWithoutLeap(i64::from(self.number(1, 365)?))
        } else {
            RuleDay::Julian(i64::from(self.number(0, 365)?))
        };
        let mut basis = Basis::Wall;
        let seconds = if self.eat(b'/').is_some() {
            let seconds = self.hms(167)?;
            if let Some(c) = self.0.first() {
                let kind = match c {
                    b'w' => Some(Basis::Wall),
                    b's' => Some(Basis::Standard),
                    b'u' | b'g' | b'z' => Some(Basis::Utc),
                    _ => None,
                };
                if let Some(kind) = kind {
                    basis = kind;
                    self.0 = &self.0[1..];
                }
            }
            seconds
        } else {
            7200
        };
        Some(Transition {
            day,
            seconds,
            basis,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i64, m: usize, d: i64, h: i64) -> i64 {
        (days_from_year(y) + (0..m - 1).map(|m| month_days(y, m)).sum::<i64>() + d - 1) * DAY
            + h * 3600
    }

    #[test]
    fn daylight_saving_uses_earlier_instant_for_overlap_and_prior_offset_for_gap() {
        let zone = Zone {
            future: Rule::parse("EST5EDT,M3.2.0,M11.1.0"),
            ..Zone::default()
        };
        assert_eq!(zone.offset(utc(2024, 3, 10, 6)), -5 * 3600);
        assert_eq!(zone.offset(utc(2024, 3, 10, 7)), -4 * 3600);
        assert_eq!(zone.wall_offset(utc(2024, 3, 10, 2) * 1000), -300);
        assert_eq!(zone.wall_offset(utc(2024, 3, 10, 3) * 1000), -240);
        assert_eq!(zone.wall_offset(utc(2024, 11, 3, 1) * 1000), -240);
        assert_eq!(zone.wall_offset(utc(2024, 11, 3, 2) * 1000), -300);
    }

    #[test]
    fn southern_hemisphere_rules_can_change_by_half_an_hour() {
        let rule = Rule::parse("<+1030>-10:30<+11>-11,M10.1.0,M4.1.0").unwrap();
        assert_eq!(rule.offset(utc(2050, 1, 1, 0)), 660 * 60);
        assert_eq!(rule.offset(utc(2050, 7, 1, 0)), 630 * 60);
    }

    #[test]
    fn tzif_reads_utc_transitions_and_the_future_rule() {
        let mut bytes = Vec::new();
        for width in [4, 8] {
            bytes.extend_from_slice(b"TZif2");
            bytes.extend_from_slice(&[0; 15]);
            for n in [0_u32, 0, 0, 1, 2, 0] {
                bytes.extend(n.to_be_bytes());
            }
            bytes.extend(std::iter::repeat_n(0, width));
            bytes.push(1);
            for n in [-14_400_i32, -18_000] {
                bytes.extend(n.to_be_bytes());
                bytes.extend([0, 0]);
            }
        }
        bytes.extend(b"\nEST5EDT,M3.2.0,M11.1.0\n");
        let zone = Zone::tzif(&bytes).unwrap();
        assert_eq!(zone.offset(-1), -14_400);
        assert_eq!(zone.offset(0), -18_000);
        assert_eq!(zone.offset(utc(2050, 1, 1, 0)), -18_000);
        assert_eq!(zone.offset(utc(2050, 7, 1, 0)), -14_400);
    }
}
