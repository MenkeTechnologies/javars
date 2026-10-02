//! `java.util.BitSet`, ported from `java.base/java/util/BitSet.java`
//! (openjdk 27): a growable vector of 64-bit words with the JDK's capacity
//! rule (`size()` is the allocated word count times 64, doubling on growth),
//! its index checks and their messages, its `hashCode`, and its `{1, 3}`
//! rendering.

/// One `java.util.BitSet`.
#[derive(Clone, Debug)]
pub struct BitSet {
    /// The allocated words; `words.len()` is what `size()` reports.
    words: Vec<u64>,
    /// `sizeIsSticky`: the caller chose the size, so `clone` keeps it.
    sticky: bool,
}

/// An index check's failure: the `IndexOutOfBoundsException` (or, for the
/// constructor, `NegativeArraySizeException`) message.
pub type Refusal = (&'static str, String);

fn word_index(bit: usize) -> usize {
    bit >> 6
}

fn check_index(i: i64) -> Result<usize, Refusal> {
    if i < 0 {
        Err(("IndexOutOfBoundsException", format!("bitIndex < 0: {i}")))
    } else {
        Ok(i as usize)
    }
}

fn check_range(from: i64, to: i64) -> Result<(usize, usize), Refusal> {
    if from < 0 {
        return Err((
            "IndexOutOfBoundsException",
            format!("fromIndex < 0: {from}"),
        ));
    }
    if to < 0 {
        return Err(("IndexOutOfBoundsException", format!("toIndex < 0: {to}")));
    }
    if from > to {
        return Err((
            "IndexOutOfBoundsException",
            format!("fromIndex: {from} > toIndex: {to}"),
        ));
    }
    Ok((from as usize, to as usize))
}

impl Default for BitSet {
    fn default() -> Self {
        BitSet::new()
    }
}

impl BitSet {
    /// `new BitSet()`: one word.
    pub fn new() -> Self {
        BitSet {
            words: vec![0],
            sticky: false,
        }
    }

    /// `new BitSet(nbits)`.
    pub fn with_bits(nbits: i64) -> Result<Self, Refusal> {
        if nbits < 0 {
            return Err(("NegativeArraySizeException", format!("nbits < 0: {nbits}")));
        }
        let n = if nbits == 0 {
            0
        } else {
            word_index(nbits as usize - 1) + 1
        };
        Ok(BitSet {
            words: vec![0; n],
            sticky: true,
        })
    }

    /// `wordsInUse`: one past the last non-zero word.
    fn in_use(&self) -> usize {
        self.words
            .iter()
            .rposition(|&w| w != 0)
            .map_or(0, |i| i + 1)
    }

    /// `ensureCapacity`: grow to at least `need` words, doubling.
    fn ensure(&mut self, need: usize) {
        if self.words.len() < need {
            let request = (2 * self.words.len()).max(need);
            self.words.resize(request, 0);
            self.sticky = false;
        }
    }

    fn bit(&self, i: usize) -> bool {
        self.words
            .get(word_index(i))
            .is_some_and(|w| w & (1u64 << (i & 63)) != 0)
    }

    fn put(&mut self, i: usize, on: bool) {
        if on {
            self.ensure(word_index(i) + 1);
            self.words[word_index(i)] |= 1u64 << (i & 63);
        } else if let Some(w) = self.words.get_mut(word_index(i)) {
            *w &= !(1u64 << (i & 63));
        }
    }

    pub fn get(&self, i: i64) -> Result<bool, Refusal> {
        Ok(self.bit(check_index(i)?))
    }

    pub fn set(&mut self, i: i64, on: bool) -> Result<(), Refusal> {
        let i = check_index(i)?;
        self.put(i, on);
        Ok(())
    }

    pub fn flip(&mut self, i: i64) -> Result<(), Refusal> {
        let i = check_index(i)?;
        self.ensure(word_index(i) + 1);
        let on = !self.bit(i);
        self.put(i, on);
        Ok(())
    }

    /// `set(from, to, on)` / `clear(from, to)` / `flip(from, to)` over
    /// `[from, to)`; `op` maps each old bit to its new one.
    pub fn range(&mut self, from: i64, to: i64, op: impl Fn(bool) -> bool) -> Result<(), Refusal> {
        let (from, to) = check_range(from, to)?;
        if from == to {
            return Ok(());
        }
        // A range that only clears never grows the set, as the JDK's
        // `clear(from, to)` trims `toIndex` to `length()` first.
        let grows = op(false);
        let to = if grows {
            to
        } else {
            to.min(self.words.len() * 64)
        };
        if grows {
            self.ensure(word_index(to - 1) + 1);
        }
        for i in from..to {
            let v = op(self.bit(i));
            self.put(i, v);
        }
        Ok(())
    }

    /// `clear()`.
    pub fn clear_all(&mut self) {
        self.words.iter_mut().for_each(|w| *w = 0);
    }

    /// `get(from, to)`: the bits `[from, to)` shifted down to 0, in a set
    /// sized as `new BitSet(to - from)` is.
    pub fn slice(&self, from: i64, to: i64) -> Result<BitSet, Refusal> {
        let (from, to) = check_range(from, to)?;
        let len = self.length() as usize;
        if len <= from || from == to {
            return BitSet::with_bits(0);
        }
        let to = to.min(len);
        let mut out = BitSet::with_bits((to - from) as i64)?;
        for i in from..to {
            if self.bit(i) {
                out.put(i - from, true);
            }
        }
        Ok(out)
    }

    /// `nextSetBit(from)`: `-1` when there is none.
    pub fn next_set(&self, from: i64) -> Result<i64, Refusal> {
        if from < 0 {
            return Err((
                "IndexOutOfBoundsException",
                format!("fromIndex < 0: {from}"),
            ));
        }
        let end = self.in_use() * 64;
        Ok((from as usize..end)
            .find(|&i| self.bit(i))
            .map_or(-1, |i| i as i64))
    }

    /// `nextClearBit(from)`.
    pub fn next_clear(&self, from: i64) -> Result<i64, Refusal> {
        if from < 0 {
            return Err((
                "IndexOutOfBoundsException",
                format!("fromIndex < 0: {from}"),
            ));
        }
        let mut i = from as usize;
        while self.bit(i) {
            i += 1;
        }
        Ok(i as i64)
    }

    /// `previousSetBit(from)` / `previousClearBit(from)`: `-1` is accepted and
    /// answers `-1`; anything lower is refused.
    pub fn previous(&self, from: i64, set: bool) -> Result<i64, Refusal> {
        if from < 0 {
            if from == -1 {
                return Ok(-1);
            }
            return Err((
                "IndexOutOfBoundsException",
                format!("fromIndex < -1: {from}"),
            ));
        }
        Ok((0..=from as usize)
            .rev()
            .find(|&i| self.bit(i) == set)
            .map_or(-1, |i| i as i64))
    }

    /// `length()`: one past the highest set bit.
    pub fn length(&self) -> i64 {
        match self.in_use() {
            0 => 0,
            n => (64 * (n - 1) + 64 - self.words[n - 1].leading_zeros() as usize) as i64,
        }
    }

    /// `size()`: the allocated bits.
    pub fn size(&self) -> i64 {
        (self.words.len() * 64) as i64
    }

    pub fn cardinality(&self) -> i64 {
        self.words.iter().map(|w| i64::from(w.count_ones())).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.in_use() == 0
    }

    /// `and`/`or`/`xor`/`andNot` with another set, word by word, growing as
    /// the JDK's `or`/`xor` do.
    pub fn combine(&mut self, other: &BitSet, op: fn(u64, u64) -> u64) {
        let need = other.in_use();
        if op(0, 1) != 0 {
            self.ensure(need);
        }
        for i in 0..self.words.len() {
            let o = other.words.get(i).copied().unwrap_or(0);
            self.words[i] = op(self.words[i], o);
        }
    }

    pub fn intersects(&self, other: &BitSet) -> bool {
        self.words.iter().zip(&other.words).any(|(a, b)| a & b != 0)
    }

    /// `hashCode()`.
    pub fn hash(&self) -> i32 {
        let mut h: i64 = 1234;
        for i in (0..self.in_use()).rev() {
            h ^= (self.words[i] as i64).wrapping_mul(i as i64 + 1);
        }
        ((h >> 32) ^ h) as i32
    }

    pub fn same_bits(&self, other: &BitSet) -> bool {
        let n = self.in_use();
        n == other.in_use() && self.words[..n] == other.words[..n]
    }

    /// `clone()`: a set whose size the caller did not choose is trimmed to
    /// the words in use.
    pub fn cloned(&self) -> BitSet {
        let mut c = self.clone();
        if !c.sticky {
            let n = c.in_use();
            c.words.truncate(n);
        }
        c
    }

    /// The set bits, ascending — `stream()`'s elements.
    pub fn ones(&self) -> Vec<i64> {
        (0..self.in_use() * 64)
            .filter(|&i| self.bit(i))
            .map(|i| i as i64)
            .collect()
    }

    /// `toString()`: `{1, 3, 5}`.
    pub fn render(&self) -> String {
        let parts: Vec<String> = self.ones().iter().map(i64::to_string).collect();
        format!("{{{}}}", parts.join(", "))
    }
}
