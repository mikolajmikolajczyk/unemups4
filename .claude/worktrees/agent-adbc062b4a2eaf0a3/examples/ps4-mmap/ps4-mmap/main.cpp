#include <orbis/libkernel.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h> // Standard header, or define constants below

// --- Constants (FreeBSD/PS4 values) ---
#ifndef MAP_FIXED
#define MAP_FIXED     0x0010
#endif
#ifndef MAP_ANON
#define MAP_ANON      0x1000
#endif
#ifndef MAP_PRIVATE
#define MAP_PRIVATE   0x0002
#endif
#ifndef PROT_READ
#define PROT_READ     0x01
#define PROT_WRITE    0x02
#define PROT_EXEC     0x04
#endif

// Helper to check results
void assert_msg(int condition, const char* msg) {
    if (condition) {
        sceKernelDebugOutText(0, "[PASS] ");
    } else {
        sceKernelDebugOutText(0, "[FAIL] ");
    }
    sceKernelDebugOutText(0, msg);
    sceKernelDebugOutText(0, "\n");
}

int main(void) {
    sceKernelDebugOutText(0, "--- Advanced MMAP Test ---\n");

    size_t size = 4096; // 1 Page

    // TEST 1: Basic Anonymous Map (Happy Path)
    void* ptr1 = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    assert_msg(ptr1 != MAP_FAILED, "Basic Allocation");
    
    if (ptr1 != MAP_FAILED) {
        // Write test
        *(int*)ptr1 = 0xDEADBEEF;
        assert_msg(*(int*)ptr1 == 0xDEADBEEF, "Memory Write/Read");
    }

    // TEST 2: MAP_FIXED (Request specific address)
    // Let's try an address generally safe in 64-bit space, e.g., 0x400000000 (16GB mark)
    // Note: Your LinuxMemoryManager starts heap here, so let's pick something higher to avoid collision
    // with the previous malloc/mmap. Let's try 0x600000000 (24GB).
    void* fixed_addr = (void*)0x600000000;
    void* ptr2 = mmap(fixed_addr, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_FIXED, -1, 0);
    
    char msg[128];
    snprintf(msg, sizeof(msg), "Fixed Map Request: %p -> Got: %p", fixed_addr, ptr2);
    assert_msg(ptr2 == fixed_addr, msg);

    // TEST 3: Unmap and Reuse
    if (ptr2 == fixed_addr) {
        int ret = munmap(ptr2, size);
        assert_msg(ret == 0, "Unmap memory");

        // Try mapping SAME address again immediately
        void* ptr3 = mmap(fixed_addr, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_FIXED, -1, 0);
        assert_msg(ptr3 == fixed_addr, "Reuse unmapped address");
    }

    // TEST 4: JIT Memory (PROT_EXEC)
    // This tests if your MemoryProtection logic correctly passes the EXEC bit
    void* ptr_exec = mmap(NULL, size, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANON, -1, 0);
    assert_msg(ptr_exec != MAP_FAILED, "Allocate Executable Memory (JIT)");
    
    // Note: We cannot test WRITING to ptr_exec here because it is Read-Exec.
    // Writing would crash the emulator (which is correct behavior, but ends the test early).
    // We also cannot test executing it easily without constructing valid x86 code.
    // Just verifying the syscall accepted the flag is enough for now.

    sceKernelDebugOutText(0, "--- Test Finished ---\n");
    return 0;
}