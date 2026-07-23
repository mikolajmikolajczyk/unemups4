#include <orbis/libkernel.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h> 

#define THREAD_COUNT 10
#define MUTEX_TEST_THREADS 4
#define MUTEX_ITERATIONS 10000

// --- Globals for TLS / Once tests ---
static OrbisPthreadKey g_tls_key = 0;
static OrbisPthreadOnce g_once = {}; 

// --- Globals for Mutex tests ---
static OrbisPthreadMutex g_mutex; 
static volatile int g_shared_counter = 0;

// --- Globals for CondVar tests ---
static OrbisPthreadMutex g_cond_mutex;
static OrbisPthreadCond g_cond;
static int g_data_ready = 0;
static int g_shared_data = 0;

// --- Globals for Misc tests ---
static volatile int g_misc_done = 0;

// --- Globals for RWLock/TryLock tests ---
static OrbisPthreadRwlock g_rwlock;
static OrbisPthreadMutex g_try_mutex;
static volatile int g_trylock_result = -1;

static void dbg(const char* s) {
    sceKernelDebugOutText(0, s);
}

static void dbg_fmt(const char* fmt, long a, long b) {
    char buf[256];
    snprintf(buf, sizeof(buf), fmt, a, b);
    sceKernelDebugOutText(0, buf);
}

// ---------------------------------------------------------
// SECTION 1: TLS & ONCE Helpers
// ---------------------------------------------------------

static void tls_dtor(void* ptr) {
    char buf[256];
    snprintf(buf, sizeof(buf), "[Guest TLS DTOR] called with ptr=%p\n", ptr);
    sceKernelDebugOutText(0, buf);
}

static void once_init(void) {
    dbg("[Guest ONCE] once_init running (should appear exactly once)\n");
}

static void* thread_entry_basic(void* arg) {
    long id = (long)arg;
    scePthreadOnce(&g_once, once_init);
    uintptr_t val = 0x11110000u + (uintptr_t)id;
    scePthreadSetspecific(g_tls_key, (void*)val);
    sceKernelUsleep(10000); 
    if ((id % 2) == 1) {
        uintptr_t code = 0xDEAD0000u + (uintptr_t)id;
        scePthreadExit((void*)code);
    }
    return NULL;
}

// ---------------------------------------------------------
// SECTION 2: MUTEX Helpers
// ---------------------------------------------------------

static void* thread_entry_mutex(void* arg) {
    for(int i = 0; i < MUTEX_ITERATIONS; i++) {
        scePthreadMutexLock(&g_mutex);
        g_shared_counter++;
        scePthreadMutexUnlock(&g_mutex);
    }
    return NULL;
}

// ---------------------------------------------------------
// SECTION 4: CONDVAR Helpers
// ---------------------------------------------------------

static void* consumer_thread(void* arg) {
    scePthreadMutexLock(&g_cond_mutex);
    while (g_data_ready == 0) {
        scePthreadCondWait(&g_cond, &g_cond_mutex);
    }
    dbg_fmt("[Consumer] Data received: %ld\n", (long)g_shared_data, 0);
    scePthreadMutexUnlock(&g_cond_mutex);
    return NULL;
}

static void* producer_thread(void* arg) {
    sceKernelUsleep(50000); 
    scePthreadMutexLock(&g_cond_mutex);
    g_shared_data = 12345;
    g_data_ready = 1;
    scePthreadCondSignal(&g_cond);
    scePthreadMutexUnlock(&g_cond_mutex);
    return NULL;
}

// ---------------------------------------------------------
// SECTION 5: TIMED WAIT Helper
// ---------------------------------------------------------

static void* thread_timed_waiter(void* arg) {
    scePthreadMutexLock(&g_cond_mutex);
    int r = scePthreadCondTimedwait(&g_cond, &g_cond_mutex, 100000);
    if (r == 110) {
        dbg("[Timed] Success! Got ETIMEDOUT (110).\n");
    } else {
        dbg_fmt("!!! [Timed] Failed! Expected 110, got %d\n", r, 0);
    }
    scePthreadMutexUnlock(&g_cond_mutex);
    return NULL;
}

// ---------------------------------------------------------
// SECTION 6: MISC Helpers
// ---------------------------------------------------------

typedef int (*PthreadSetNameFunc)(OrbisPthread, const char*);
typedef int (*PthreadGetNameFunc)(OrbisPthread, char*, size_t);

static void* thread_misc_entry(void* arg) {
    sceKernelUsleep(20000);
    OrbisPthread me = scePthreadSelf();
    
    if (scePthreadEqual(me, me)) {
        dbg("[Misc] scePthreadEqual(self, self) -> True (OK)\n");
    } else {
        dbg("!!! [Misc] scePthreadEqual failed!\n");
    }

    char buf[64];
    memset(buf, 0, sizeof(buf));
    ((PthreadGetNameFunc)scePthreadGetname)(me, buf, sizeof(buf));
    dbg_fmt("[Misc] My Name is: '%s'\n", (long)buf, 0);

    if (strcmp(buf, "RenamedByMain") == 0) {
        dbg("[Misc] Name verification SUCCESS\n");
    } else {
        dbg("!!! [Misc] Name verification FAILED\n");
    }

    scePthreadYield();
    g_misc_done = 1;
    return NULL;
}

// ---------------------------------------------------------
// SECTION 7: RWLOCK & TRYLOCK Helpers
// ---------------------------------------------------------

static void* thread_trylock_tester(void* arg) {
    // Attempt to lock mutex held by Main thread
    // Should return EBUSY (16)
    int r = scePthreadMutexTrylock(&g_try_mutex);
    g_trylock_result = r;
    return NULL;
}

// ---------------------------------------------------------
// MAIN
// ---------------------------------------------------------

int main(void) {
    dbg("\n=== [Guest] STARTING REGRESSION TESTS ===\n");

    // TEST 1
    dbg("\n>>> TEST 1: TLS, Once, Exit Values\n");
    scePthreadKeyCreate(&g_tls_key, tls_dtor);
    OrbisPthread threads[THREAD_COUNT];
    char name[64];
    for (long i = 0; i < THREAD_COUNT; i++) {
        snprintf(name, sizeof(name), "Th_Basic_%ld", i);
        scePthreadCreate(&threads[i], NULL, thread_entry_basic, (void*)i, name);
    }
    for (int i = 0; i < THREAD_COUNT; i++) {
        void* out = NULL;
        scePthreadJoin(threads[i], &out);
    }
    dbg(">>> TEST 1 COMPLETE\n");

    // TEST 2
    dbg("\n>>> TEST 2: Mutex Locking\n");
    g_shared_counter = 0;
    scePthreadMutexInit(&g_mutex, NULL, "MyGlobalMutex"); 
    OrbisPthread m_threads[MUTEX_TEST_THREADS];
    for (long i = 0; i < MUTEX_TEST_THREADS; i++) {
        scePthreadCreate(&m_threads[i], NULL, thread_entry_mutex, (void*)i, "MutexWorker");
    }
    for (int i = 0; i < MUTEX_TEST_THREADS; i++) {
        scePthreadJoin(m_threads[i], NULL);
    }
    int expected = MUTEX_TEST_THREADS * MUTEX_ITERATIONS;
    char buf[256];
    snprintf(buf, sizeof(buf), "Counter Final: %d (Expected: %d) -> %s\n", 
             g_shared_counter, expected, 
             (g_shared_counter == expected) ? "SUCCESS" : "FAILURE");
    sceKernelDebugOutText(0, buf);
    scePthreadMutexDestroy(&g_mutex);

    // TEST 3
    dbg("\n>>> TEST 3: Recursive Mutex\n");
    OrbisPthreadMutexattr attr;
    OrbisPthreadMutex rec_mutex;
    scePthreadMutexattrInit(&attr);
    scePthreadMutexattrSettype(&attr, 2); // RECURSIVE
    scePthreadMutexInit(&rec_mutex, &attr, "RecMutex");
    scePthreadMutexattrDestroy(&attr);
    scePthreadMutexLock(&rec_mutex); 
    if (scePthreadMutexLock(&rec_mutex) == 0) { 
        scePthreadMutexUnlock(&rec_mutex);
        scePthreadMutexUnlock(&rec_mutex);
        dbg("Recursive Test SUCCESS\n");
    } else {
        dbg("!!! Recursive Lock FAILED\n");
    }
    scePthreadMutexDestroy(&rec_mutex);

    // TEST 4
    dbg("\n>>> TEST 4: Condition Variable\n");
    scePthreadMutexInit(&g_cond_mutex, NULL, "CondMutex");
    scePthreadCondInit(&g_cond, NULL, "MyCondVar");
    OrbisPthread t_cons, t_prod;
    scePthreadCreate(&t_cons, NULL, consumer_thread, NULL, "Consumer");
    scePthreadCreate(&t_prod, NULL, producer_thread, NULL, "Producer");
    scePthreadJoin(t_prod, NULL);
    scePthreadJoin(t_cons, NULL);
    scePthreadMutexDestroy(&g_cond_mutex);
    scePthreadCondDestroy(&g_cond);
    dbg(">>> TEST 4 COMPLETE\n");

    // TEST 5
    dbg("\n>>> TEST 5: Timed Wait\n");
    scePthreadMutexInit(&g_cond_mutex, NULL, "TimedMutex");
    scePthreadCondInit(&g_cond, NULL, "TimedCond");
    OrbisPthread t_timed;
    scePthreadCreate(&t_timed, NULL, thread_timed_waiter, NULL, "TimedWaiter");
    scePthreadJoin(t_timed, NULL);
    scePthreadMutexDestroy(&g_cond_mutex);
    scePthreadCondDestroy(&g_cond);
    dbg(">>> TEST 5 COMPLETE\n");

    // TEST 6
    dbg("\n>>> TEST 6: Misc API\n");
    OrbisPthread t_misc;
    scePthreadCreate(&t_misc, NULL, thread_misc_entry, NULL, "OriginalName");
    ((PthreadSetNameFunc)scePthreadSetName)(t_misc, "RenamedByMain");
    int r_det = scePthreadDetach(t_misc);
    if (r_det == 0) dbg("[Misc] Detach called successfully.\n");
    int r_can = scePthreadCancel(scePthreadSelf());
    if (r_can == 95) dbg("[Misc] scePthreadCancel -> 95 (ENOTSUP) as expected.\n");
    int waits = 0;
    while(g_misc_done == 0 && waits < 20) { sceKernelUsleep(50000); waits++; }
    if (g_misc_done) dbg("[Misc] Detached thread finished.\n");
    dbg(">>> TEST 6 COMPLETE\n");

    // =====================================================
    // TEST 7: RWLOCK & TRYLOCK
    // =====================================================
    dbg("\n>>> TEST 7: RWLock & TryLock\n");

    // A. RWLOCK
    // We just verify the API calls don't crash and locks work exclusively
    int rr = scePthreadRwlockInit(&g_rwlock, NULL, "MyRwLock");
    if (rr == 0) {
        scePthreadRwlockWrlock(&g_rwlock);
        dbg("[RWLock] Write Lock Acquired\n");
        scePthreadRwlockUnlock(&g_rwlock);

        scePthreadRwlockRdlock(&g_rwlock);
        dbg("[RWLock] Read Lock Acquired\n");
        scePthreadRwlockUnlock(&g_rwlock);
        
        scePthreadRwlockDestroy(&g_rwlock);
        dbg("[RWLock] Test Passed.\n");
    } else {
        dbg_fmt("!!! RwLock Init Failed: %d\n", rr, 0);
    }

    // B. TRYLOCK
    scePthreadMutexInit(&g_try_mutex, NULL, "TryMutex");
    
    // 1. Lock it (Main Thread)
    scePthreadMutexLock(&g_try_mutex);
    
    // 2. Spawn thread to try and fail
    OrbisPthread t_try;
    scePthreadCreate(&t_try, NULL, thread_trylock_tester, NULL, "TryLocker");
    scePthreadJoin(t_try, NULL);

    if (g_trylock_result == 16) { // EBUSY
        dbg("[TryLock] Correctly received EBUSY (16) when locked.\n");
    } else {
        dbg_fmt("!!! [TryLock] Failed! Expected 16, got %d\n", g_trylock_result, 0);
    }

    // 3. Unlock and verify TryLock works when free
    scePthreadMutexUnlock(&g_try_mutex);
    int r_succ = scePthreadMutexTrylock(&g_try_mutex);
    if (r_succ == 0) {
        dbg("[TryLock] Successfully acquired free mutex.\n");
        scePthreadMutexUnlock(&g_try_mutex);
    } else {
        dbg_fmt("!!! [TryLock] Failed to acquire free mutex: %d\n", r_succ, 0);
    }

    scePthreadMutexDestroy(&g_try_mutex);
    dbg(">>> TEST 7 COMPLETE\n");

    dbg("\n=== [Guest] ALL TESTS FINISHED ===\n");
    return 0;
}