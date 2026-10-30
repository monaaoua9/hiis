// Random query strings built from the syntax characters and a few real words:
// nothing may panic or hang, and the different ways of running a query must agree.
mod common;
use common::*;
use hiis::{Config, Engine};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

const PIECES: &[&str] = &[
    "fox",
    "dog",
    "lazy",
    "quick",
    "brown",
    "posting",
    "segment",
    "compres*",
    "qiuck~",
    "lazzy~2",
    "dog~0",
    "+",
    "-",
    "(",
    ")",
    "\"",
    "\"quick fox\"",
    "\"lazy dog\"~3",
    "\"brown\"~1",
    "^2",
    "^0",
    "^",
    "~",
    "*",
    "AND",
    "OR",
    "NOT",
    " ",
    " ",
    " ",
    "fox^3",
    "(fox OR dog)",
    "+(lazy quick)^2",
    "-cat",
    "~~",
    "((",
    "))",
    "\u{0645}\u{062f}\u{0631}\u{0633}\u{0629}",
    "\"",
    "1.5",
    "^99999999999",
];

fn random_query(r: &mut Rng) -> String {
    let n = 1 + r.next() % 8;
    let mut q = String::new();
    for _ in 0..n {
        q.push_str(r.pick(PIECES));
        if r.next() % 3 != 0 {
            q.push(' ');
        }
    }
    q
}

#[test]
fn random_queries_never_panic_and_agree_with_each_other() {
    let (_d, e) = loaded(small_cfg());
    let mut r = Rng(0x2545f4914f6cdd1d);
    let mut non_empty = 0;

    for _ in 0..1500 {
        let q = random_query(&mut r);
        let hits = e.search(&q, 5).unwrap_or_else(|err| panic!("{q:?}: {err}"));
        assert_eq!(hits, e.search_unpruned(&q, 5).unwrap(), "pruned vs exhaustive on {q:?}");

        let all = e.search(&q, 1000).unwrap();
        assert_eq!(e.count(&q).unwrap(), all.len() as u64, "count on {q:?}");
        assert_eq!(&all[..all.len().min(5)], &hits[..], "top-5 is a prefix of the full ranking on {q:?}");
        if !all.is_empty() {
            non_empty += 1;
            let x = e.explain(&q, all[0].doc_id).unwrap().unwrap();
            assert_eq!(x.score, all[0].score, "explain on {q:?}");
            let _ = e.highlight(all[0].doc_id, &q, &hiis::HighlightOpts::default());
        }
        let _ = e.did_you_mean(&q);
    }
    // the generator has to produce queries that actually match things, or this proves nothing
    assert!(non_empty > 300, "only {non_empty} queries matched");
}

#[test]
fn pagination_covers_random_queries() {
    let dir = TmpDir::new();
    let e = Engine::open(&dir.0, Config { flush_docs: 3, ..small_cfg() }).unwrap();
    for d in CORPUS.iter().cycle().take(40) {
        e.add_document(d).unwrap();
    }
    e.commit().unwrap();

    let mut r = Rng(77);
    for _ in 0..200 {
        let q = random_query(&mut r);
        let full = e.search(&q, 1000).unwrap();
        let mut got = Vec::new();
        let mut cur = None;
        for _ in 0..200 {
            let page = e.search_page(&q, 7, cur).unwrap();
            got.extend(page.hits);
            match page.next {
                Some(c) => cur = Some(c),
                None => break,
            }
        }
        assert_eq!(got, full, "{q:?}");
    }
}

#[test]
fn garbage_bytes_as_queries() {
    let (_d, e) = loaded(small_cfg());
    let mut r = Rng(5);
    for _ in 0..500 {
        let len = r.next() % 40;
        let s: String = (0..len).map(|_| char::from_u32((r.next() % 0x2fff) as u32).unwrap_or('?')).collect();
        let _ = e.search(&s, 3).unwrap();
        let _ = e.did_you_mean(&s);
        let _ = e.complete(&s, 3);
    }
}
