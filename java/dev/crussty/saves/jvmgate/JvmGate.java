package dev.crussty.saves.jvmgate;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.zip.Deflater;

/**
 * JVM gate (v0.9.61): the REAL Java side of the container, end to end.
 *
 * Runs in TWO separate JVM processes against the SAME container dir:
 *   write  — attach, competitive parallel writes through pure JNI
 *            (byte[] -> GetPrimitiveArrayCritical -> Rust zstd tiers),
 *            then the begin/end save barrier (fsync). Exit = server death.
 *   verify — a FRESH JVM process (the new server) reopens the container and
 *            proves every chunk SEMANTICALLY through a real NBT parser:
 *            every section's every block on its exact position, root tags
 *            intact; then hammers concurrent reads for latency percentiles.
 *
 * Every call goes through the production natives (natives::n_*) — the same
 * registry the Paper shim binds; only the JVM-side marshalling is new.
 *
 * Chunk payloads are real chunk NBT the Java side builds itself: sections
 * {Y, block_states{palette, data}, biomes{palette}}, 4-bit packed indices
 * (LSB-first, 16 per long — the vanilla layout), deterministic xorshift
 * content, ~5-8 KiB per chunk. The store re-encodes sections canonically
 * (palette order is semantically neutral), so byte equality is NOT the gate:
 * the gate is block-position equality over a real NBT parse.
 */
public final class JvmGate {

    static {
        String lib = System.getProperty("csaves.lib");
        if (lib == null || lib.isEmpty()) {
            System.err.println("FAIL: -Dcsaves.lib=<path to libc_saves.so> is required");
            System.exit(2);
        }
        System.load(lib);
    }

    // ---- the production surface (mirrors SavesShim exactly) ----
    static native boolean nActive();
    static native int nAttach(String kind, String level, String dir);
    static native byte[] nReadChunk(int handle, int x, int z);
    static native boolean nWriteChunk(int handle, int x, int z, byte[] data, boolean fresh);
    static native boolean nBeginSave();
    static native boolean nEndSave();
    static native void nMarkAutosave();

    // ------------------------------------------------------------------
    // chunk generation: the Java form of the microtest generator
    // ------------------------------------------------------------------

    static String[] paletteOf(int i, long round, int y) {
        int npal = 3 + (int) (((i + round + y) % 5));
        String[] names = new String[npal];
        for (int p = 0; p < npal; p++) {
            names[p] = String.format("minecraft:blk_%03d",
                    (((long) i * 31L + round * 7L + p + y) % 900L));
        }
        Arrays.sort(names);
        return names;
    }

    static int[] entriesOf(int i, long round, int y, int npal) {
        long st = ((long) i * 0x100000001B3L + round + y * 31L) | 1L;
        int[] entries = new int[4096];
        for (int j = 0; j < 4096; j++) {
            st ^= (st << 13);
            st ^= (st >>> 7);
            st ^= (st << 17);
            entries[j] = (int) (Long.remainderUnsigned(st, npal));
        }
        return entries;
    }

    static long[] packLsb(int[] entries, int bits) {
        int per = 64 / bits;
        long[] out = new long[(entries.length + per - 1) / per];
        for (int j = 0; j < entries.length; j++) {
            out[j / per] |= ((long) entries[j]) << ((j % per) * bits);
        }
        return out;
    }

    /** Fully deterministic expected block grid: names[Y][position]. */
    static String[][] expectedGrid(int i, long round) {
        String[][] grid = new String[8][];
        for (int y = 0; y < 8; y++) {
            String[] pal = paletteOf(i, round, y);
            int[] entries = entriesOf(i, round, y, pal.length);
            String[] names = new String[4096];
            for (int j = 0; j < 4096; j++) names[j] = pal[entries[j]];
            grid[y] = names;
        }
        return grid;
    }

    // ------------------------------------------------------------------
    // minimal NBT writer (the subset the generator needs)
    // ------------------------------------------------------------------

    static final byte TAG_END = 0, TAG_BYTE = 1, TAG_SHORT = 2, TAG_INT = 3,
            TAG_LONG = 4, TAG_FLOAT = 5, TAG_DOUBLE = 6, TAG_BYTEARR = 7,
            TAG_STRING = 8, TAG_LIST = 9, TAG_COMPOUND = 10, TAG_INTARR = 11,
            TAG_LONGARR = 12;

    static void wString(ByteArrayOutputStream o, String s) {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        o.write((b.length >>> 8) & 0xFF);
        o.write(b.length & 0xFF);
        o.writeBytes(b);
    }

    static void wTagHeader(ByteArrayOutputStream o, byte type, String name) {
        o.write(type);
        wString(o, name);
    }

    static byte[] chunkNbt(int i, long round) {
        ByteArrayOutputStream o = new ByteArrayOutputStream(4096);
        wTagHeader(o, TAG_COMPOUND, ""); // root
        wTagHeader(o, TAG_STRING, "Status");
        wString(o, "minecraft:full");
        wTagHeader(o, TAG_LONG, "InhabitedTime");
        o.writeBytes(new byte[]{0, 0, 0, 0, 0, 0, 0, 0}); // long 0
        wTagHeader(o, TAG_INT, "xPos");
        o.writeBytes(new byte[]{(byte) (i >> 24), (byte) (i >> 16), (byte) (i >> 8), (byte) i});
        wTagHeader(o, TAG_INT, "zPos");
        o.writeBytes(new byte[]{(byte) (-i >> 24), (byte) (-i >> 16), (byte) (-i >> 8), (byte) -i});

        wTagHeader(o, TAG_LIST, "sections");
        o.write(TAG_COMPOUND);
        o.writeBytes(new byte[]{0, 0, 0, 8}); // 8 sections (i32 length)
        for (int y = 0; y < 8; y++) {
            wTagHeader(o, TAG_BYTE, "Y");
            o.write(y);
            wTagHeader(o, TAG_COMPOUND, "block_states");
            String[] pal = paletteOf(i, round, y);
            wTagHeader(o, TAG_LIST, "palette");
            o.write(TAG_STRING);
            o.writeBytes(new byte[]{0, 0, 0, (byte) pal.length});
            for (String s : pal) wString(o, s);
            long[] packed = packLsb(entriesOf(i, round, y, pal.length), 4);
            wTagHeader(o, TAG_LONGARR, "data");
            o.writeBytes(new byte[]{(byte) (packed.length >>> 24),
                    (byte) (packed.length >>> 16), (byte) (packed.length >>> 8),
                    (byte) packed.length});
            for (long v : packed) {
                o.writeBytes(new byte[]{(byte) (v >>> 56), (byte) (v >>> 48),
                        (byte) (v >>> 40), (byte) (v >>> 32),
                        (byte) (v >>> 24), (byte) (v >>> 16),
                        (byte) (v >>> 8), (byte) v});
            }
            o.write(TAG_END); // end block_states
            wTagHeader(o, TAG_COMPOUND, "biomes");
            wTagHeader(o, TAG_LIST, "palette");
            o.write(TAG_STRING);
            o.writeBytes(new byte[]{0, 0, 0, 1});
            wString(o, "minecraft:plains");
            o.write(TAG_END); // end biomes
            o.write(TAG_END); // end section
        }
        o.write(TAG_END); // end root
        return o.toByteArray();
    }

    // ------------------------------------------------------------------
    // minimal NBT parser (what verify needs: the full walk)
    // ------------------------------------------------------------------

    static final class NbtReader {
        final byte[] b;
        int p;

        NbtReader(byte[] b) {
            this.b = b;
        }

        int u8() { return b[p++] & 0xFF; }

        int u16() { int v = ((b[p] & 0xFF) << 8) | (b[p + 1] & 0xFF); p += 2; return v; }

        int i32() {
            int v = ((b[p] & 0xFF) << 24) | ((b[p + 1] & 0xFF) << 16)
                    | ((b[p + 2] & 0xFF) << 8) | (b[p + 3] & 0xFF);
            p += 4;
            return v;
        }

        long i64() {
            long v = 0;
            for (int k = 0; k < 8; k++) v = (v << 8) | (b[p + k] & 0xFF);
            p += 8;
            return v;
        }

        String str() {
            int len = u16();
            String s = new String(b, p, len, StandardCharsets.UTF_8);
            p += len;
            return s;
        }

        Object payload(byte t) {
            switch (t) {
                case TAG_BYTE: return (byte) u8();
                case TAG_SHORT: return (short) u16();
                case TAG_INT: return i32();
                case TAG_LONG: return i64();
                case TAG_FLOAT: case TAG_DOUBLE: {
                    int n = (t == TAG_FLOAT) ? 4 : 8;
                    p += n; // light fields the gate never inspects
                    return Float.NaN;
                }
                case TAG_INTARR: {
                    int n = i32();
                    int[] a = new int[n];
                    for (int k = 0; k < n; k++) a[k] = i32();
                    return a;
                }
                case TAG_BYTEARR: {
                    int n = i32();
                    byte[] a = Arrays.copyOfRange(b, p, p + n);
                    p += n;
                    return a;
                }
                case TAG_STRING: return str();
                case TAG_LIST: {
                    byte et = (byte) u8();
                    int n = i32();
                    List<Object> l = new ArrayList<>(Math.max(0, Math.min(n, 1024)));
                    for (int k = 0; k < n; k++) l.add(payload(et));
                    return l;
                }
                case TAG_COMPOUND: {
                    Map<String, Object> m = new HashMap<>();
                    while (true) {
                        byte et = (byte) u8();
                        if (et == TAG_END) break;
                        String name = str();
                        m.put(name, payload(et));
                    }
                    return m;
                }
                case TAG_LONGARR: {
                    int n = i32();
                    long[] a = new long[n];
                    for (int k = 0; k < n; k++) a[k] = i64();
                    return a;
                }
                default:
                    throw new IllegalStateException("unsupported tag " + t + " at " + p);
            }
        }

        @SuppressWarnings("unchecked")
        Map<String, Object> parseRoot() {
            byte t = (byte) u8();
            if (t != TAG_COMPOUND) throw new IllegalStateException("root not compound");
            str(); // root name
            return (Map<String, Object>) payload(t);
        }
    }

    /** Unwrap a section's block grid: palette[value] at every position. */
    @SuppressWarnings("unchecked")
    static String[] sectionGrid(Map<String, Object> sec) {
        Map<String, Object> bs = (Map<String, Object>) sec.get("block_states");
        List<Object> pal = (List<Object>) bs.get("palette");
        long[] data = (long[]) bs.get("data");
        String[] names = pal.toArray(new String[0]);
        int bits = Math.max(4, 32 - Integer.numberOfLeadingZeros(names.length - 1));
        int per = 64 / bits;
        int mask = (1 << bits) - 1;
        String[] out = new String[4096];
        for (int j = 0; j < 4096; j++) {
            int v = (int) ((data[j / per] >>> ((j % per) * bits)) & mask);
            out[j] = names[v];
        }
        return out;
    }

    /** The semantic gate: root tags + every block of every section in place. */
    static String verifyChunk(byte[] got, String[][] expected, int i) {
        Map<String, Object> root;
        try {
            root = new NbtReader(got).parseRoot();
        } catch (Exception e) {
            return "parse failed: " + e;
        }
        if (!"minecraft:full".equals(root.get("Status"))) return "Status changed";
        if (!Integer.valueOf(i).equals(root.get("xPos"))) return "xPos changed";
        List<Object> secs = (List<Object>) root.get("sections");
        if (secs == null || secs.size() != 8) return "sections missing";
        Map<Integer, Map<String, Object>> byY = new HashMap<>();
        for (Object o : secs) {
            Map<String, Object> sec = (Map<String, Object>) o;
            byY.put(((Byte) sec.get("Y")).intValue(), sec);
        }
        for (int y = 0; y < 8; y++) {
            Map<String, Object> sec = byY.get(y);
            if (sec == null) return "section Y=" + y + " missing";
            String[] gotGrid;
            try {
                gotGrid = sectionGrid(sec);
            } catch (Exception e) {
                return "section Y=" + y + " grid failed: " + e;
            }
            String[] exp = expected[y];
            for (int j = 0; j < 4096; j++) {
                if (!exp[j].equals(gotGrid[j])) {
                    return String.format("Y=%d pos=%d: %s != %s", y, j, gotGrid[j], exp[j]);
                }
            }
        }
        return null;
    }

    // ------------------------------------------------------------------
    // harness
    // ------------------------------------------------------------------

    static int cxOf(int i, int side) { return (i / side) - side / 2; }
    static int czOf(int i, int side) { return (i % side) - side / 2; }

    interface Task {
        void run(int i) throws Exception;
    }

    static void parallel(int n, int threads, Task task) throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        AtomicInteger cursor = new AtomicInteger(0);
        CountDownLatch done = new CountDownLatch(threads);
        final Exception[] failure = new Exception[1];
        for (int t = 0; t < threads; t++) {
            pool.submit(() -> {
                try {
                    while (failure[0] == null) {
                        int i = cursor.getAndIncrement();
                        if (i >= n) break;
                        task.run(i);
                    }
                } catch (Exception e) {
                    failure[0] = e;
                } finally {
                    done.countDown();
                }
            });
        }
        done.await();
        pool.shutdown();
        if (failure[0] != null) {
            throw failure[0];
        }
    }

    static long pctl(List<Long> lat, double p) {
        List<Long> s = new ArrayList<>(lat);
        Collections.sort(s);
        int idx = (int) Math.min(s.size() - 1, (long) (s.size() * p));
        return s.get(idx);
    }

    public static void main(String[] args) throws Exception {
        String mode = args[0];
        String dir = args[1];
        int side = Integer.parseInt(args[2]);
        int threads = Integer.parseInt(args[3]);
        int rounds = args.length > 4 ? Integer.parseInt(args[4]) : 2;
        int n = side * side;

        int handle = nAttach("region", "jvmgate", dir);
        if (handle < 0) {
            System.out.println("FAIL: nAttach returned " + handle);
            System.exit(2);
        }
        if (!nActive()) {
            System.out.println("FAIL: nActive() is false after attach");
            System.exit(2);
        }
        System.out.println("jvm gate: attached store #" + handle + " dir=" + dir
                + " (" + mode + " phase, " + threads + " threads)");

        if (mode.equals("write")) {
            // optional debug: dump the first chunk's NBT for the Rust parser
            String dump = System.getProperty("csaves.dump");
            if (dump != null) {
                java.nio.file.Files.write(java.nio.file.Path.of(dump), chunkNbt(0, 0));
            }
            // null = NO_DATA through the real JNI channel, before anything exists
            byte[] absent = nReadChunk(handle, 100_000, 100_000);
            if (absent != null) {
                System.out.println("FAIL: absent chunk read must be null (NO_DATA), got "
                        + absent.length + " bytes");
                System.exit(2);
            }

            long t0 = System.nanoTime();
            parallel(n, threads, i -> {
                if (!nWriteChunk(handle, cxOf(i, side), czOf(i, side), chunkNbt(i, 0), false)) {
                    throw new RuntimeException("write FALSE at " + i);
                }
            });
            long wallMs = (System.nanoTime() - t0) / 1_000_000;
            long meanUs = wallMs * 1000L / Math.max(1, n);
            System.out.println("jvm gate: wrote " + n + " chunks (" + threads
                    + " threads) in " + (wallMs / 1000.0) + " s (mean " + meanUs + " us/write)");

            if (!nBeginSave() || !nEndSave()) {
                System.out.println("FAIL: save barrier returned false");
                System.exit(2);
            }
            nMarkAutosave();
            System.out.println("jvm gate: save barrier (begin/end fsync) ok — JVM exits (server death)");
            System.out.println("=== jvm gate: write phase PASS ===");
            System.exit(0);
        }

        if (mode.equals("verify")) {
            // the semantic gate, fresh JVM process (the new server)
            long t0 = System.nanoTime();
            AtomicInteger good = new AtomicInteger(0);
            List<String> bad = Collections.synchronizedList(new ArrayList<>());
            long payloadBytes = 0;
            parallel(n, threads, i -> {
                byte[] expectedRaw = chunkNbt(i, 0);
                byte[] got = nReadChunk(handle, cxOf(i, side), czOf(i, side));
                if (got == null) {
                    bad.add("chunk " + i + " (cx=" + cxOf(i, side) + " cz=" + czOf(i, side)
                            + ") read NULL");
                    return;
                }
                String err = verifyChunk(got, expectedGrid(i, 0), i);
                if (err == null) {
                    good.incrementAndGet();
                } else {
                    bad.add("chunk " + i + ": " + err);
                }
            });
            for (int i = 0; i < 64; i++) payloadBytes += chunkNbt(i, 0).length;
            double meanPayload = payloadBytes / 64.0;
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("jvm gate: semantic verify " + good.get() + "/" + n
                    + " chunks across the restart (NBT parse + block-position equality, "
                    + (ms / 1000.0) + " s, mean payload ~" + (long) meanPayload + " B)");
            if (good.get() != n) {
                for (String s : bad.subList(0, Math.min(8, bad.size()))) {
                    System.out.println("FAIL: " + s);
                }
                System.out.println("=== jvm gate: FAIL ===");
                System.exit(2);
            }

            // concurrent read burst — the live server read path, hot container
            List<Long> lat = Collections.synchronizedList(new ArrayList<>(n * rounds));
            long tb = System.nanoTime();
            for (int r = 0; r < rounds; r++) {
                parallel(n, threads, i -> {
                    long a = System.nanoTime();
                    byte[] got = nReadChunk(handle, cxOf(i, side), czOf(i, side));
                    long us = (System.nanoTime() - a) / 1000;
                    if (got == null) throw new IllegalStateException("burst read null at " + i);
                    lat.add(us);
                });
            }
            long burstUs = (System.nanoTime() - tb) / 1000;
            final long[] total = {0};
            parallel(64, 4, i -> {
                byte[] got = nReadChunk(handle, cxOf(i, side), czOf(i, side));
                synchronized (total) { total[0] += got.length; }
            });
            long meanB = total[0] / 64;
            long p50 = pctl(lat, 0.50), p95 = pctl(lat, 0.95), p99 = pctl(lat, 0.99);
            long max = Collections.max(lat);
            double mean = 0;
            for (long v : lat) mean += v;
            mean /= Math.max(1, lat.size());
            System.out.println("jvm gate: read burst " + (n * rounds) + " JNI reads ("
                    + threads + " threads, " + rounds + " rounds): mean="
                    + String.format("%.1f", mean) + "us p50=" + p50 + " p95=" + p95
                    + " p99=" + p99 + " max=" + max + "us wall=" + (burstUs / 1e6)
                    + " s (mean chunk " + meanB + " B)");
            System.out.println("jvm gate: end save barrier " + nEndSave() + ", autosave marker ok");
            System.out.println("=== jvm gate: PASS ===");
            System.exit(0);
        }

        System.out.println("FAIL: unknown mode " + mode);
        System.exit(2);
    }
}
