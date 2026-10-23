// Regenerate javars's `java.lang.Character` property data from a real JDK.
//
//   JAVA_HOME=/path/to/jdk "$JAVA_HOME/bin/java" scripts/GenUnicodeTables.java
//
// run from the crate root. It writes:
//
//   src/unicode_tables.rs  — sorted, non-overlapping (first, last, value) runs
//                            of Character.getType, getNumericValue,
//                            isIdeographic and the emoji properties, and the Character.UnicodeBlock
//                            runs `getName`'s fallback names come from;
//   src/data/uninames.bin  — the names Character.getName answers that are not
//                            that fallback, word-dictionary encoded:
//                              u32 word count, then each word as u8 length +
//                              bytes; u32 name count, then each name as a
//                              3-byte big-endian code point, a u8 word count
//                              and that many u16 big-endian word indexes
//                              (the words join with single spaces).
//
// Every value is the JDK's own answer for that code point, so the data is
// exactly as current as the JDK that ran this.
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

public class GenUnicodeTables {
    interface F { int f(int cp); }

    static void runs(StringBuilder out, String name, String doc, F f, int dflt) {
        out.append(doc).append("#[rustfmt::skip]\n").append("pub static ").append(name).append(": &[(u32, u32, i32)] = &[\n");
        int start = 0, cur = f.f(0);
        for (int cp = 1; cp <= 0x110000; cp++) {
            int v = cp <= 0x10FFFF ? f.f(cp) : Integer.MIN_VALUE;
            if (v != cur) {
                if (cur != dflt) out.append("    (0x").append(Integer.toHexString(start)).append(", 0x").append(Integer.toHexString(cp - 1)).append(", ").append(cur).append("),\n");
                start = cp;
                cur = v;
            }
        }
        out.append("];\n\n");
    }

    static String blockName(int cp) {
        Character.UnicodeBlock b = Character.UnicodeBlock.of(cp);
        return b == null ? null : b.toString().replace('_', ' ');
    }

    static String fallback(int cp) {
        String b = blockName(cp);
        return b == null ? null : b + " " + Integer.toHexString(cp).toUpperCase(Locale.ROOT);
    }

    public static void main(String[] a) throws IOException {
        StringBuilder out = new StringBuilder();
        out.append("//! Unicode property tables for `java.lang.Character`, generated from\n//! `openjdk ")
            .append(System.getProperty("java.version"))
            .append("` by `scripts/GenUnicodeTables.java`, which dumps the JDK's own\n//! answers for every code point (`Character.getType`, `getNumericValue`,\n//! `isIdeographic`, the emoji properties, `UnicodeBlock.of`) as sorted, non-overlapping\n//! `(first, last, value)` runs. Regenerate rather than edit.\n\n");
        runs(out, "GENERAL_CATEGORY", "/// `Character.getType`: runs of one general category (the JDK's byte codes).\n/// Unlisted code points are `UNASSIGNED` (0).\n", Character::getType, 0);
        runs(out, "NUMERIC_VALUE", "/// `Character.getNumericValue`: runs of one value; unlisted code points\n/// answer -1 (no numeric value).\n", Character::getNumericValue, -1);
        runs(out, "IDEOGRAPHIC", "/// `Character.isIdeographic`: runs answering 1; unlisted code points are\n/// not ideographic.\n", cp -> Character.isIdeographic(cp) ? 1 : 0, 0);
        runs(out, "EMOJI", "/// The `Character` emoji properties as bits: 1 `isEmoji`, 2\n/// `isEmojiPresentation`, 4 `isEmojiModifier`, 8 `isEmojiModifierBase`, 16\n/// `isEmojiComponent`, 32 `isExtendedPictographic`. Unlisted code points have\n/// none.\n", cp -> (Character.isEmoji(cp) ? 1 : 0) | (Character.isEmojiPresentation(cp) ? 2 : 0) | (Character.isEmojiModifier(cp) ? 4 : 0) | (Character.isEmojiModifierBase(cp) ? 8 : 0) | (Character.isEmojiComponent(cp) ? 16 : 0) | (Character.isExtendedPictographic(cp) ? 32 : 0), 0);
        // Blocks, as runs of one name; a code point in no block is unlisted.
        out.append("/// `Character.UnicodeBlock.of`: runs of one block, by the name its\n/// `toString()` gives with `_` read as a space — the prefix of the name\n/// `Character.getName` falls back to. Unlisted code points are in no block.\n");
        out.append("#[rustfmt::skip]\npub static BLOCKS: &[(u32, u32, &str)] = &[\n");
        int start = 0;
        String cur = blockName(0);
        for (int cp = 1; cp <= 0x110000; cp++) {
            String v = cp <= 0x10FFFF ? blockName(cp) : "\0end";
            if (!Objects.equals(v, cur)) {
                if (cur != null) out.append("    (0x").append(Integer.toHexString(start)).append(", 0x").append(Integer.toHexString(cp - 1)).append(", \"").append(cur).append("\"),\n");
                start = cp;
                cur = v;
            }
        }
        out.append("];\n\n");
        out.append(LOOKUP);
        Files.writeString(Path.of("src/unicode_tables.rs"), out.toString());

        // The explicit names, word-dictionary encoded.
        Map<String, Integer> index = new LinkedHashMap<>();
        List<int[]> names = new ArrayList<>();
        List<Integer> cps = new ArrayList<>();
        for (int cp = 0; cp <= 0x10FFFF; cp++) {
            String n = Character.getName(cp);
            if (n == null || n.equals(fallback(cp))) continue;
            String[] words = n.split(" ", -1);
            int[] ids = new int[words.length];
            for (int i = 0; i < words.length; i++) ids[i] = index.computeIfAbsent(words[i], w -> index.size());
            names.add(ids);
            cps.add(cp);
        }
        ByteArrayOutputStream bin = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(bin);
        d.writeInt(index.size());
        for (String w : index.keySet()) {
            byte[] b = w.getBytes(StandardCharsets.UTF_8);
            d.writeByte(b.length);
            d.write(b);
        }
        d.writeInt(names.size());
        for (int i = 0; i < names.size(); i++) {
            int cp = cps.get(i);
            d.writeByte(cp >> 16);
            d.writeByte(cp >> 8);
            d.writeByte(cp);
            d.writeByte(names.get(i).length);
            for (int id : names.get(i)) d.writeShort(id);
        }
        Files.createDirectories(Path.of("src/data"));
        Files.write(Path.of("src/data/uninames.bin"), bin.toByteArray());
        System.err.println(index.size() + " words, " + names.size() + " names, " + bin.size() + " bytes");
    }

    static final String LOOKUP = """
        /// The value `table` gives the code point `cp`, or `default` when no run
        /// covers it.
        pub fn lookup<T: Copy>(table: &[(u32, u32, T)], cp: u32, default: T) -> T {
            match table.binary_search_by(|&(lo, hi, _)| {
                if hi < cp {
                    std::cmp::Ordering::Less
                } else if lo > cp {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            }) {
                Ok(i) => table[i].2,
                Err(_) => default,
            }
        }
        """;
}
