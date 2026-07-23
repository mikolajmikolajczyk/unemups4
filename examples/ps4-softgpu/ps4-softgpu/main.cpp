#include <orbis/libkernel.h>
#include <orbis/VideoOut.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    sceKernelDebugOutText(0, "[GUEST] Double Buffering Test\n");

    const int width = 1920;
    const int height = 1080;
    size_t bufferSize = width * height * 4;
    
    // 1. Allocate TWO buffers
    uint32_t* buffer0 = (uint32_t*)malloc(bufferSize);
    uint32_t* buffer1 = (uint32_t*)malloc(bufferSize);

    int handle = sceVideoOutOpen(0xFF, 1, 0, 0);

    // 2. Register both buffers
    // Register Buffer 0 at Index 0
    void* list0[] = { buffer0 };
    sceVideoOutRegisterBuffers(handle, 0, list0, 1, 0);

    // Register Buffer 1 at Index 1
    void* list1[] = { buffer1 };
    sceVideoOutRegisterBuffers(handle, 1, list1, 1, 0);

    uint32_t* buffers[] = { buffer0, buffer1 };
    int currentBufferIdx = 0; // We draw to this one
    int frame = 0;

    while (1) {
        // Double Buffering Logic:
        // We draw to 'currentBufferIdx'. 
        // The screen is currently showing '1 - currentBufferIdx' (the other one).
        
        uint32_t* drawTarget = buffers[currentBufferIdx];
        
        // --- DRAWING LOGIC (Same as before) ---
        uint32_t bgColor = (frame % 120 < 60) ? 0xFF0000FF : 0xFFFF0000; // Blue/Red
        
        for (int i = 0; i < width * height; i++) {
            drawTarget[i] = bgColor;
        }

        int boxSize = 100;
        //int boxX = (frame * 8) % (width - boxSize);
        //int boxY = 400;
        int boxX = (frame * 5) % (width - boxSize);
        int boxY = (frame * 3) % (height - boxSize);
        for (int y = 0; y < boxSize; y++) {
            for (int x = 0; x < boxSize; x++) {
                drawTarget[(boxY + y) * width + (boxX + x)] = 0xFFFFFFFF; // White box
            }
        }

        // --- FLIP ---
        // We submit the buffer we just drew.
        sceVideoOutSubmitFlip(handle, currentBufferIdx, 1, 0);

        // Swap for next frame
        currentBufferIdx = 1 - currentBufferIdx;
        
        frame++;
       // sceKernelUsleep(16666);
    }
    return 0;
}