package dev.crussty.saves;

import ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController.IORunnable;
import net.minecraft.world.level.chunk.storage.RegionFile;

/** No-op commit closure for our WriteData: the c-saves container already
 * holds the chunk, so nothing is written to the region file. The instance
 * doubles as the marker the module uses to recognise its own WriteData. */
public final class SavesShimNoop implements IORunnable {
    public static final SavesShimNoop INSTANCE = new SavesShimNoop();

    private SavesShimNoop() {}

    @Override
    public void run(RegionFile regionFile) {
        // c-saves owns persistence; region file commit is intentionally skipped.
    }
}
