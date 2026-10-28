//! Porter (1980) stemmer, ported from the reference ANSI C implementation
//! including its two published departures (`bli`->`ble`, `logi`->`log`).
//! Index arithmetic is signed because `j` legitimately reaches -1 when a
//! suffix covers the whole word.

struct Stemmer {
    b: Vec<u8>,
    k: isize,
    j: isize,
}

pub fn stem(word: String) -> String {
    if word.len() <= 2 || !word.bytes().all(|c| c.is_ascii_lowercase()) {
        return word;
    }
    let mut s = Stemmer { b: word.as_bytes().to_vec(), k: word.len() as isize - 1, j: 0 };
    s.step1ab();
    if s.k > 0 {
        s.step1c();
        s.step2();
        s.step3();
        s.step4();
        s.step5();
    }
    String::from_utf8_lossy(&s.b[..=s.k as usize]).into_owned()
}

impl Stemmer {
    fn at(&self, i: isize) -> u8 {
        self.b[i as usize]
    }

    fn cons(&self, i: isize) -> bool {
        match self.at(i) {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => i == 0 || !self.cons(i - 1),
            _ => true,
        }
    }

    // number of VC sequences in b[0..=j]
    fn m(&self) -> i32 {
        let mut n = 0;
        let mut i = 0;
        loop {
            if i > self.j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > self.j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > self.j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    fn vowel_in_stem(&self) -> bool {
        (0..=self.j).any(|i| !self.cons(i))
    }

    fn doublec(&self, j: isize) -> bool {
        j >= 1 && self.at(j) == self.at(j - 1) && self.cons(j)
    }

    fn cvc(&self, i: isize) -> bool {
        if i < 2 || !self.cons(i) || self.cons(i - 1) || !self.cons(i - 2) {
            return false;
        }
        !matches!(self.at(i), b'w' | b'x' | b'y')
    }

    fn ends(&mut self, s: &str) -> bool {
        let s = s.as_bytes();
        let len = s.len() as isize;
        if len > self.k + 1 {
            return false;
        }
        let from = (self.k - len + 1) as usize;
        if &self.b[from..=self.k as usize] != s {
            return false;
        }
        self.j = self.k - len;
        true
    }

    fn set_to(&mut self, s: &str) {
        self.b.truncate((self.j + 1) as usize);
        self.b.extend_from_slice(s.as_bytes());
        self.k = self.b.len() as isize - 1;
    }

    fn r(&mut self, s: &str) {
        if self.m() > 0 {
            self.set_to(s);
        }
    }

    fn step1ab(&mut self) {
        if self.at(self.k) == b's' {
            if self.ends("sses") {
                self.k -= 2;
            } else if self.ends("ies") {
                self.set_to("i");
            } else if self.at(self.k - 1) != b's' {
                self.k -= 1;
            }
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.k -= 1;
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.k = self.j;
            if self.ends("at") {
                self.set_to("ate");
            } else if self.ends("bl") {
                self.set_to("ble");
            } else if self.ends("iz") {
                self.set_to("ize");
            } else if self.doublec(self.k) {
                self.k -= 1;
                if matches!(self.at(self.k), b'l' | b's' | b'z') {
                    self.k += 1;
                }
            } else if self.m() == 1 && self.cvc(self.k) {
                self.set_to("e");
            }
        }
    }

    fn step1c(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            self.b[self.k as usize] = b'i';
        }
    }

    fn step2(&mut self) {
        match self.at(self.k - 1) {
            b'a' => {
                if self.ends("ational") {
                    self.r("ate");
                } else if self.ends("tional") {
                    self.r("tion");
                }
            }
            b'c' => {
                if self.ends("enci") {
                    self.r("ence");
                } else if self.ends("anci") {
                    self.r("ance");
                }
            }
            b'e' => {
                if self.ends("izer") {
                    self.r("ize");
                }
            }
            b'l' => {
                if self.ends("bli") {
                    self.r("ble");
                } else if self.ends("alli") {
                    self.r("al");
                } else if self.ends("entli") {
                    self.r("ent");
                } else if self.ends("eli") {
                    self.r("e");
                } else if self.ends("ousli") {
                    self.r("ous");
                }
            }
            b'o' => {
                if self.ends("ization") {
                    self.r("ize");
                } else if self.ends("ation") || self.ends("ator") {
                    self.r("ate");
                }
            }
            b's' => {
                if self.ends("alism") {
                    self.r("al");
                } else if self.ends("iveness") {
                    self.r("ive");
                } else if self.ends("fulness") {
                    self.r("ful");
                } else if self.ends("ousness") {
                    self.r("ous");
                }
            }
            b't' => {
                if self.ends("aliti") {
                    self.r("al");
                } else if self.ends("iviti") {
                    self.r("ive");
                } else if self.ends("biliti") {
                    self.r("ble");
                }
            }
            b'g' if self.ends("logi") => {
                self.r("log");
            }
            _ => {}
        }
    }

    fn step3(&mut self) {
        match self.at(self.k) {
            b'e' => {
                if self.ends("icate") {
                    self.r("ic");
                } else if self.ends("ative") {
                    self.r("");
                } else if self.ends("alize") {
                    self.r("al");
                }
            }
            b'i' => {
                if self.ends("iciti") {
                    self.r("ic");
                }
            }
            b'l' => {
                if self.ends("ical") {
                    self.r("ic");
                } else if self.ends("ful") {
                    self.r("");
                }
            }
            b's' if self.ends("ness") => {
                self.r("");
            }
            _ => {}
        }
    }

    fn step4(&mut self) {
        let hit = match self.at(self.k - 1) {
            b'a' => self.ends("al"),
            b'c' => self.ends("ance") || self.ends("ence"),
            b'e' => self.ends("er"),
            b'i' => self.ends("ic"),
            b'l' => self.ends("able") || self.ends("ible"),
            b'n' => self.ends("ant") || self.ends("ement") || self.ends("ment") || self.ends("ent"),
            b'o' => {
                (self.ends("ion") && self.j >= 0 && matches!(self.at(self.j), b's' | b't')) || self.ends("ou")
            }
            b's' => self.ends("ism"),
            b't' => self.ends("ate") || self.ends("iti"),
            b'u' => self.ends("ous"),
            b'v' => self.ends("ive"),
            b'z' => self.ends("ize"),
            _ => false,
        };
        if hit && self.m() > 1 {
            self.k = self.j;
        }
    }

    fn step5(&mut self) {
        self.j = self.k;
        if self.at(self.k) == b'e' {
            let a = self.m();
            if a > 1 || (a == 1 && !self.cvc(self.k - 1)) {
                self.k -= 1;
            }
        }
        if self.at(self.k) == b'l' && self.doublec(self.k) && self.m() > 1 {
            self.k -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn porter_examples() {
        let cases = [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("falling", "fall"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("running", "run"),
            ("generalization", "gener"),
            ("effective", "effect"),
            ("probate", "probat"),
            ("controlling", "control"),
            ("indexing", "index"),
            ("searches", "search"),
        ];
        for (w, want) in cases {
            assert_eq!(stem(w.to_string()), want, "stem({w})");
        }
    }

    #[test]
    fn leaves_short_and_non_ascii_alone() {
        assert_eq!(stem("is".into()), "is");
        assert_eq!(stem("café".into()), "café");
        assert_eq!(stem("abc123".into()), "abc123");
    }
}
