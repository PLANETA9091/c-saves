package dev.crussty.saves.agent;

import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;

/**
 * c-saves standalone agent (v0.9.62): a REAL Paper server plays from the
 * container without the CRUSSTY kernel.
 *
 * premain: load the native engine (-Dcsaves.lib=.../libc_saves.so), register
 * a ClassFileTransformer that rewrites RegionFileStorage's six chunk IO entry
 * points (ASM replace_body, see AgentAsm) into static bridges on CrusstyShim,
 * and install the shutdown hook that runs the durable end-save barrier and
 * prints the served/miss/written stats the CI gate asserts on.
 *
 * The natives resolve through STANDARD JNI exports (Java_dev_crussty_saves_
 * agent_CrusstyShim_n*) — one-line pass-throughs into the same natives::n_*
 * registry the kernel path binds. Zero duplicated engine logic.
 *
 * Disable with -Dcsaves.agent.off=true (vanilla comparison runs).
 */
public final class CrusstyAgent {

    private static final String RFS = "net/minecraft/world/level/chunk/storage/RegionFileStorage";
    private static final String RFS_DOT = "net.minecraft.world.level.chunk.storage.RegionFileStorage";
    private static final String RD = "Lca/spottedleaf/moonrise/patches/chunk_system/io/"
            + "MoonriseRegionFileIO$RegionDataController$ReadData;";
    private static final String WD = "Lca/spottedleaf/moonrise/patches/chunk_system/io/"
            + "MoonriseRegionFileIO$RegionDataController$WriteData;";
    private static final String CPT = "Lnet/minecraft/nbt/CompoundTag;";
    private static final String CPOS = "Lnet/minecraft/world/level/ChunkPos;";
    private static final String STV = "Lnet/minecraft/nbt/StreamTagVisitor;";

    private CrusstyAgent() {}

    public static void premain(String args, Instrumentation inst) {
        if (Boolean.getBoolean("csaves.agent.off")) {
            System.err.println("[c-saves] agent disabled (csaves.agent.off) — vanilla IO");
            return;
        }
        String lib = System.getProperty("csaves.lib");
        if (lib == null || lib.isEmpty()) {
            System.err.println("[c-saves] FAIL: -Dcsaves.lib=<libc_saves.so> is required");
            throw new IllegalStateException("csaves.lib missing");
        }
        System.load(lib);
        System.err.println("[c-saves] agent: native engine loaded from " + lib);

        Runtime.getRuntime().addShutdownHook(new Thread(CrusstyShim::dumpStats, "csaves-stats"));

        inst.addTransformer(new Transformer(), true);
        System.err.println("[c-saves] agent: transformer installed (target: " + RFS_DOT + ")");
    }

    /** Rewrites the six RegionFileStorage IO bodies into CrusstyShim bridges. */
    static final class Transformer implements ClassFileTransformer {
        private static final String DC =
                "ca/spottedleaf/moonrise/patches/chunk_system/io/datacontroller/";
        // bridgeDesc for the controller-level write entry: (Object,II,CompoundTag)WriteData
        private static final String DC_BRIDGE = "(Ljava/lang/Object;II" + CPT + ")" + WD;
        // {className, {methodName, methodDesc, bridgeName, bridgeDesc, args}...}
        private static final Object[][] TARGETS = {
            { RFS, RFS_SPECS },
            { DC + "ChunkDataController", new Object[][] {
                { "startWrite", "(II" + CPT + ")" + WD, "dcStartWrite", DC_BRIDGE,
                  new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' }, { 3, 'L' } } } } },
            { DC + "EntityDataController", new Object[][] {
                { "startWrite", "(II" + CPT + ")" + WD, "dcStartWrite", DC_BRIDGE,
                  new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' }, { 3, 'L' } } } } },
            { DC + "PoiDataController", new Object[][] {
                { "startWrite", "(II" + CPT + ")" + WD, "dcStartWrite", DC_BRIDGE,
                  new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' }, { 3, 'L' } } } } },
            // v0.9.64 DIAGNOSTIC PROBES (-Dcsaves.probe): method-entry wraps on
            // the Moonrise scheduler's load-vs-generate decision points. The
            // original bodies run untouched after the log call.
            { "ca/spottedleaf/moonrise/patches/chunk_system/scheduling/ChunkTaskScheduler",
              new Object[][] {
                { "scheduleChunkLoad", "(IILnet/minecraft/world/level/chunk/status/ChunkStatus;Z"
                    + "Lca/spottedleaf/concurrentutil/util/Priority;Ljava/util/function/Consumer;)V",
                  "probeSCL4", "(IILjava/lang/Object;Z)V",
                  new int[][] { { 1, 'I' }, { 2, 'I' }, { 3, 'L' }, { 4, 'Z' } }, "prepend" },
                { "scheduleChunkLoad", "(IIZLnet/minecraft/world/level/chunk/status/ChunkStatus;Z"
                    + "Lca/spottedleaf/concurrentutil/util/Priority;Ljava/util/function/Consumer;)V",
                  "probeSCL7", "(IIZLjava/lang/Object;ZZ)V",
                  new int[][] { { 1, 'I' }, { 2, 'I' }, { 3, 'Z' }, { 4, 'L' }, { 5, 'Z' } }, "prepend" },
                { "syncLoadNonFull", "(IILnet/minecraft/world/level/chunk/status/ChunkStatus;)"
                    + "Lnet/minecraft/world/level/chunk/ChunkAccess;",
                  "probeSync3", "(IILjava/lang/Object;)V",
                  new int[][] { { 1, 'I' }, { 2, 'I' }, { 3, 'L' } }, "prepend" },
                { "beginChunkLoadForNonFullSync", "(IILnet/minecraft/world/level/chunk/status/ChunkStatus;"
                    + "Lca/spottedleaf/concurrentutil/util/Priority;)Z",
                  "probeBCL4", "(IILjava/lang/Object;Ljava/lang/Object;)V",
                  new int[][] { { 1, 'I' }, { 2, 'I' }, { 3, 'L' }, { 4, 'L' } }, "prepend" },
                { "schedule", "(IILnet/minecraft/world/level/chunk/status/ChunkStatus;"
                    + "Lca/spottedleaf/moonrise/patches/chunk_system/scheduling/NewChunkHolder;"
                    + "Ljava/util/List;Lca/spottedleaf/concurrentutil/util/Priority;)"
                    + "Lca/spottedleaf/moonrise/patches/chunk_system/scheduling/task/ChunkProgressionTask;",
                  "probeSched6", "(IILjava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
                  new int[][] { { 1, 'I' }, { 2, 'I' }, { 3, 'L' }, { 4, 'L' }, { 5, 'L' }, { 6, 'L' } }, "prepend" } } },
            { "net/minecraft/server/level/ServerChunkCache",
              new Object[][] {
                { "syncLoad", "(IILnet/minecraft/world/level/chunk/status/ChunkStatus;)"
                    + "Lnet/minecraft/world/level/chunk/ChunkAccess;",
                  "probeSync3", "(IILjava/lang/Object;)V",
                  new int[][] { { 1, 'I' }, { 2, 'I' }, { 3, 'L' } }, "prepend" } } },
        };

        static final boolean PROBE_ON = Boolean.getBoolean("csaves.probe");

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> beingDefined,
                ProtectionDomain pd, byte[] bytes) {
            Object[][] specs = null;
            for (Object[] t : TARGETS) {
                if (t[0].equals(className)) {
                    specs = (Object[][]) t[1];
                    break;
                }
            }
            if (specs == null) {
                return null;
            }
            String verb = "patched";
            List<String> missing = new ArrayList<>();
            byte[] out = bytes;
            int patched = 0;
            int attempted = 0;
            for (Object[] spec : specs) {
                String mode = spec.length > 5 ? (String) spec[5] : "replace";
                if (mode.equals("prepend") && !PROBE_ON) {
                    continue;
                }
                attempted++;
                byte[] next = mode.equals("prepend")
                        ? AgentAsm.prependBody(out, (String) spec[0], (String) spec[1],
                            "dev/crussty/saves/agent/CrusstyShim", (String) spec[2],
                            (String) spec[3], (int[][]) spec[4])
                        : AgentAsm.replaceBody(out, (String) spec[0], (String) spec[1],
                            "dev/crussty/saves/agent/CrusstyShim", (String) spec[2],
                            (String) spec[3], (int[][]) spec[4]);
                if (next == null) {
                    String err = AgentAsm.lastError();
                    missing.add((String) spec[0] + (err == null || err.isEmpty() ? "" : " (" + err + ")"));
                } else {
                    out = next;
                    patched++;
                }
            }
            System.err.println("[c-saves] " + verb + " " + className + ": " + patched + "/" + attempted
                    + " methods" + (missing.isEmpty() ? "" : " — MISSING: " + missing));
            // a mismatch is only when an ATTEMPTED spec failed to apply —
            // probe specs filtered by -Dcsaves.probe are deliberately absent
            if (patched < attempted) {
                System.err.println("[c-saves] FAIL: " + className + " mismatch — "
                        + "the shim signatures do not cover this Paper build");
            }
            try {
                String fname = className.replace('/', '_');
                java.nio.file.Path dump = java.nio.file.Path.of(
                        System.getProperty("csaves.dumpdir", "/tmp") + "/" + fname + ".patched.class");
                java.nio.file.Files.write(dump, out);
            } catch (Throwable t) {
                System.err.println("[c-saves] dump failed: " + t);
            }
            return out;
        }
    }

    /** The eight RegionFileStorage bodies. */
    private static final Object[][] RFS_SPECS = {
        { "moonrise$doesRegionFileNotExistNoIO", "(II)Z", "rfDoesNotExistNoIO",
          "(" + descOf(RFS) + "II)Z",
          new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' } } },
        { "moonrise$readData", "(II)" + RD, "rfReadData",
          "(" + descOf(RFS) + "II)" + RD,
          new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' } } },
        { "moonrise$finishRead", "(II" + RD + ")" + CPT, "rfFinishRead",
          "(" + descOf(RFS) + "II" + RD + ")" + CPT,
          new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' }, { 3, 'L' } } },
        { "moonrise$startWrite", "(II" + CPT + ")" + WD, "rfStartWrite",
          "(" + descOf(RFS) + "II" + CPT + ")" + WD,
          new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' }, { 3, 'L' } } },
        { "moonrise$finishWrite", "(II" + WD + ")V", "rfFinishWrite",
          "(" + descOf(RFS) + "II" + WD + ")V",
          new int[][] { { 0, 'L' }, { 1, 'I' }, { 2, 'I' }, { 3, 'L' } } },
        { "read", "(" + CPOS + ")" + CPT, "rfRead",
          "(" + descOf(RFS) + CPOS + ")" + CPT,
          new int[][] { { 0, 'L' }, { 1, 'L' } } },
        { "write", "(" + CPOS + CPT + ")V", "rfWrite",
          "(" + descOf(RFS) + CPOS + CPT + ")V",
          new int[][] { { 0, 'L' }, { 1, 'L' }, { 2, 'L' } } },
        { "scanChunk", "(" + CPOS + STV + ")V", "rfScan",
          "(" + descOf(RFS) + CPOS + STV + ")V",
          new int[][] { { 0, 'L' }, { 1, 'L' }, { 2, 'L' } } },
    };

    private static String descOf(String internalName) {
        return "L" + internalName + ";";
    }
}
