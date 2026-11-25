use super::{MIN_MS, MONTHS, clip, make_time};

struct Parsed {
    fields: [i32; 7],
    offset: Option<i32>,
}

pub(super) fn parse(text: &str) -> f64 {
    let text = text.trim_matches(' ');
    let Some(date) = iso(text.as_bytes()).or_else(|| legacy(text.as_bytes())) else {
        return f64::NAN;
    };
    let [_, month, day, hour, minute, second, millisecond] = date.fields;
    if !(0..=11).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 24
        || minute > 59
        || second > 59
        || hour == 24 && (minute | second | millisecond) != 0
    {
        return f64::NAN;
    }
    let t = make_time(date.fields.map(f64::from), date.offset.is_none());
    clip(t - f64::from(date.offset.unwrap_or(0)) * MIN_MS as f64)
}

fn iso(text: &[u8]) -> Option<Parsed> {
    let mut s = Cursor(text);
    let year = match s.peek() {
        b'+' | b'-' => {
            let negative = s.peek() == b'-';
            s.advance();
            let year = s.digits(6, 6)?;
            if negative && year == 0 {
                return None;
            }
            if negative { -year } else { year }
        }
        _ => s.digits(4, 4)?,
    };
    let mut fields = [year, 0, 1, 0, 0, 0, 0];
    if s.eat(b'-') {
        fields[1] = s.digits(2, 2)? - 1;
        if s.eat(b'-') {
            fields[2] = s.digits(2, 2)?;
        }
    }
    let mut offset = Some(0);
    if s.eat(b'T') {
        offset = None;
        fields[3] = s.digits(2, 2)?;
        s.eat(b':').then_some(())?;
        fields[4] = s.digits(2, 2)?;
        if s.eat(b':') {
            fields[5] = s.digits(2, 2)?;
            fields[6] = s.milliseconds();
        }
    }
    if !s.0.is_empty() {
        offset = Some(s.offset(true)?);
    }
    s.0.is_empty().then_some(Parsed { fields, offset })
}

const OFFSETS: &[(&[u8], i32)] = &[
    (b"GMT", 0),
    (b"UTC", 0),
    (b"UT", 0),
    (b"Z", 0),
    (b"EDT", -240),
    (b"EST", -300),
    (b"CDT", -300),
    (b"CST", -360),
    (b"MDT", -360),
    (b"MST", -420),
    (b"PDT", -420),
    (b"PST", -480),
    (b"WET", 0),
    (b"WEST", 60),
    (b"CET", 60),
    (b"CEST", 120),
    (b"EET", 120),
    (b"EEST", 180),
];

fn legacy(text: &[u8]) -> Option<Parsed> {
    let mut s = Cursor(text);
    let mut fields = [2001, 1, 1, 0, 0, 0, 0];
    let mut offset = None;
    let mut has_year = false;
    let mut has_month = false;
    let mut has_time = false;
    let mut numbers = [0; 3];
    let mut n = 0;
    while !s.0.is_empty() {
        s.skip(|c| c == b' ');
        if s.0.is_empty() {
            break;
        }
        match s.peek() {
            b'+' | b'-' => {
                let saved = s;
                if let Some(minutes) = has_time.then(|| s.offset(false)).flatten() {
                    offset = Some(minutes);
                } else {
                    s = saved;
                    let negative = s.peek() == b'-';
                    s.advance();
                    if let Some(y) = s.digits(1, usize::MAX) {
                        if negative && y == 0 {
                            return None;
                        }
                        fields[0] = if negative { -y } else { y };
                        has_year = true;
                    }
                }
            }
            b'0'..=b'9' => {
                let start = s.0.len();
                let num = s.digits(1, usize::MAX)?;
                if s.eat(b':') {
                    fields[3] = num;
                    fields[4] = s.digits(1, 2)?;
                    if s.eat(b':') {
                        fields[5] = s.digits(1, 2)?;
                        fields[6] = s.milliseconds();
                    } else if !s.0.is_empty() && s.peek() != b' ' {
                        return None;
                    }
                    has_time = true;
                } else if start - s.0.len() > 2 {
                    fields[0] = num;
                    has_year = true;
                } else if !(1..=31).contains(&num) {
                    fields[0] = short_year(num);
                    has_year = true;
                } else {
                    *numbers.get_mut(n)? = num;
                    n += 1;
                }
            }
            b'(' => {
                s.advance();
                let mut depth = 1;
                while depth != 0 {
                    match *s.0.first()? {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    s.advance();
                }
            }
            b')' => return None,
            _ => {
                if let Some(month) = MONTHS.iter().position(|month| s.word(month.as_bytes())) {
                    fields[1] = month as i32 + 1;
                    has_month = true;
                    s.skip(|c| !c.is_ascii_digit() && !b" -/(".contains(&c));
                } else if has_time && s.word(b"PM") {
                    if fields[3] != 12 {
                        fields[3] += 12;
                    }
                    continue;
                } else if has_time && s.word(b"AM") {
                    if fields[3] > 12 {
                        return None;
                    }
                    if fields[3] == 12 {
                        fields[3] = 0;
                    }
                    continue;
                } else if let Some((_, value)) = OFFSETS.iter().find(|(name, _)| s.word(name)) {
                    offset = Some(*value);
                    continue;
                } else if has_year || has_month || has_time || n != 0 {
                    return None;
                } else {
                    s.skip(|c| !b" -/(".contains(&c));
                }
            }
        }
        s.skip(|c| b"-/.,".contains(&c));
    }
    if n + usize::from(has_year) + usize::from(has_month) > 3 {
        return None;
    }
    match numbers[..n] {
        [] if !has_year => return None,
        [] => {}
        [a] if has_month => fields[2] = a,
        [a] => fields[1] = a,
        [a, b] if has_month => {
            fields[0] = short_year(b);
            fields[2] = a;
        }
        [a, b] => {
            fields[1] = a;
            fields[2] = b;
        }
        [a, b, c] => {
            fields[0] = short_year(c);
            fields[1] = a;
            fields[2] = b;
        }
        _ => unreachable!(),
    }
    fields[1] -= 1;
    Some(Parsed { fields, offset })
}

fn short_year(n: i32) -> i32 {
    n + 1900 * i32::from(n < 100) + 100 * i32::from(n < 50)
}

#[derive(Clone, Copy)]
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn peek(&self) -> u8 {
        self.0.first().copied().unwrap_or(0)
    }
    fn advance(&mut self) {
        self.0 = &self.0[1..];
    }
    fn eat(&mut self, byte: u8) -> bool {
        if self.0.first() == Some(&byte) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn skip(&mut self, test: impl Fn(u8) -> bool) {
        while self.0.first().is_some_and(|&byte| test(byte)) {
            self.advance();
        }
    }
    fn word(&mut self, word: &[u8]) -> bool {
        if self
            .0
            .get(..word.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
        {
            self.0 = &self.0[word.len()..];
            true
        } else {
            false
        }
    }
    fn digits(&mut self, min: usize, max: usize) -> Option<i32> {
        let mut count = 0;
        let mut value = 0;
        while count < max && self.peek().is_ascii_digit() {
            if value >= 100_000_000 {
                return None;
            }
            value = value * 10 + i32::from(self.peek() - b'0');
            count += 1;
            self.advance();
        }
        (count >= min).then_some(value)
    }
    fn milliseconds(&mut self) -> i32 {
        let saved = *self;
        if !self.eat(b'.') && !self.eat(b',') {
            return 0;
        }
        let mut ms = 0;
        let mut scale = 100;
        if !self.peek().is_ascii_digit() {
            *self = saved;
            return 0;
        }
        for _ in 0..9 {
            let digit = self.peek();
            if !digit.is_ascii_digit() {
                break;
            }
            ms += i32::from(digit - b'0') * scale;
            scale /= 10;
            self.advance();
        }
        ms
    }
    fn offset(&mut self, strict: bool) -> Option<i32> {
        if self.eat(b'Z') {
            return Some(0);
        }
        let sign = if self.eat(b'+') {
            1
        } else if self.eat(b'-') {
            -1
        } else {
            return None;
        };
        let start = self.0.len();
        let mut hours = self.digits(1, usize::MAX)?;
        let mut count = start - self.0.len();
        if strict && !matches!(count, 2 | 4) {
            return None;
        }
        while count > 4 {
            hours /= 100;
            count -= 2;
        }
        let minutes = if count > 2 {
            let min = hours % 100;
            hours /= 100;
            min
        } else if self.eat(b':') {
            self.digits(2, 2)?
        } else {
            0
        };
        (hours <= 23 && minutes <= 59).then_some(sign * (hours * 60 + minutes))
    }
}
