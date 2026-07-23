#include <orbis/libkernel.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/uio.h>

// --- SDK FIX (Required for printf to work correctly on some SDK versions) ---
extern "C" {
    ssize_t sceKernelWritev_patched(int fd, const struct iovec *iov, int iovcnt) __asm__("sceKernelWritev");
}

// =======================================================
// TLS VARIABLES
// =======================================================

// 1. Initialized TLS (.tdata)
// This value is stored in the ELF file. The loader must copy it to new threads.
thread_local int g_init_var = 0x13371337;

// 2. Zero-initialized TLS (.tbss)
// The loader must allocate space for this and zero it out.
thread_local int g_bss_var = 0;

void* thread_func(void* arg) {
    printf("\n[Thread 2] --- Started ---\n");
    printf("[Thread 2] Addr of g_init_var: %p\n", &g_init_var);
    printf("[Thread 2] Value of g_init_var: 0x%X\n", g_init_var);

    // TEST 1: Check Initialization
    if (g_init_var == 0x13371337) {
        printf("[Thread 2] \033[32mPASSED: .tdata initialized correctly\033[0m\n");
    } else {
        printf("[Thread 2] \033[31mFAILED: .tdata is 0x%X (Expected 0x13371337)\033[0m\n", g_init_var);
    }

    // TEST 2: Check BSS
    if (g_bss_var == 0) {
        printf("[Thread 2] \033[32mPASSED: .tbss zeroed correctly\033[0m\n");
    } else {
        printf("[Thread 2] \033[31mFAILED: .tbss is %d (Expected 0)\033[0m\n", g_bss_var);
    }

    // TEST 3: Modify Value
    g_init_var = 0xDEADBEEF;
    printf("[Thread 2] Modified g_init_var to: 0x%X\n", g_init_var);

    return NULL;
}

int main() {
    printf("--- PS4 TLS Test Suite ---\n");

    // 1. Check Main Thread State
    printf("[Main] Addr of g_init_var: %p\n", &g_init_var);
    printf("[Main] Value of g_init_var: 0x%X\n", g_init_var);

    // 2. Modify Main Thread Value
    // We set this to a unique value. If the new thread shares the same memory (bug),
    // it will see this value instead of the reset 0x13371337.
    g_init_var = 0xCAFEBABE;
    printf("[Main] Set g_init_var to: 0x%X\n", g_init_var);

    // 3. Spawn Thread
    pthread_t t;
    printf("[Main] Spawning secondary thread...\n");
    if (pthread_create(&t, NULL, thread_func, NULL) != 0) {
        printf("[Main] Failed to create thread!\n");
        return 1;
    }

    // 4. Wait for thread
    pthread_join(t, NULL);

    // 5. Verify Isolation
    printf("\n[Main] Back in main thread.\n");
    printf("[Main] Value of g_init_var: 0x%X\n", g_init_var);

    if (g_init_var == 0xCAFEBABE) {
        printf("[Main] \033[32mPASSED: Isolation (Main thread value preserved)\033[0m\n");
    } else {
        printf("[Main] \033[31mFAILED: Isolation broken! Value is 0x%X\033[0m\n", g_init_var);
    }

    printf("--- Test Finished ---\n");
    
    while(1) { sceKernelUsleep(1000000); }
    return 0;
}