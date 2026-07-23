#include <orbis/libkernel.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h> // Required for struct iovec

// --- SDK HEADER FIX ---
// You correctly found that _fs.h has the right signature (3 args), 
// but the compiler is picking up a broken 2-arg version from libkernel.h.
//
// We fix this by declaring a wrapper signature that matches _fs.h
// but links to the "sceKernelWritev" symbol in the ELF.
extern "C" {
    ssize_t sceKernelWritev_patched(int fd, const struct iovec *iov, int iovcnt) __asm__("sceKernelWritev");
}
// ----------------------

// Fallback defines if SDK headers are missing them
#ifndef O_RDONLY
#define O_RDONLY 0x0000
#define O_WRONLY 0x0001
#define O_RDWR   0x0002
#define O_CREAT  0x0200
#define O_TRUNC  0x0400
#endif

void log_test(const char* name, bool passed) {
    if (passed) {
        printf("[TEST] %s: \033[32mPASSED\033[0m\n", name);
    } else {
        printf("[TEST] %s: \033[31mFAILED\033[0m\n", name);
    }
}

int main() {
    printf("--- PS4 FS Test Suite ---\n");

    const char* path = "/app0/test_file.txt";
    const char* data = "Hello, PS4 World from HLE!";
    char buffer[128];
    int fd;
    int ret;

    // =======================================================
    // TEST 1: File Write (sceKernelOpen / Write / Close)
    // =======================================================
    printf("1. Testing sceKernelOpen (Write mode)...\n");
    
    // 0777 permissions (RWX for everyone)
    fd = sceKernelOpen(path, O_WRONLY | O_CREAT | O_TRUNC, 0777);
    
    if (fd < 0) {
        printf("Failed to open file for write. Error: %d\n", fd);
        log_test("File Write Setup", false);
    } else {
        printf("FD: %d\n", fd);
        
        size_t len = strlen(data);
        size_t written = sceKernelWrite(fd, data, len);
        
        if (written == len) {
            log_test("sceKernelWrite", true);
        } else {
            printf("Written %zu bytes, expected %zu\n", written, len);
            log_test("sceKernelWrite", false);
        }

        ret = sceKernelClose(fd);
        if (ret == 0) log_test("sceKernelClose (Write)", true);
        else log_test("sceKernelClose (Write)", false);
    }

    // =======================================================
    // TEST 2: File Read (sceKernelOpen / Read / Close)
    // =======================================================
    printf("\n2. Testing sceKernelRead...\n");
    memset(buffer, 0, sizeof(buffer));

    fd = sceKernelOpen(path, O_RDONLY, 0);
    if (fd < 0) {
        printf("Failed to open file for read. Error: %d\n", fd);
        log_test("File Read Setup", false);
    } else {
        size_t read_len = sceKernelRead(fd, buffer, sizeof(buffer) - 1);
        printf("Read content: '%s'\n", buffer);

        if (read_len == strlen(data) && strcmp(buffer, data) == 0) {
            log_test("Data Integrity Check", true);
        } else {
            log_test("Data Integrity Check", false);
        }

        sceKernelClose(fd);
    }

    // =======================================================
    // TEST 3: sceKernelWritev (Scatter-Gather)
    // =======================================================
    printf("\n3. Testing sceKernelWritev...\n");
    const char* vpath = "/app0/test_vector.txt";
    const char* header = "[HEADER]";
    const char* body   = "{BODY}";
    const char* footer = "[END]";

    fd = sceKernelOpen(vpath, O_WRONLY | O_CREAT | O_TRUNC, 0777);
    if (fd < 0) {
        log_test("Writev Open", false);
    } else {
        // Prepare IO Vectors
        struct iovec iov[3];
        
        iov[0].iov_base = (void*)header;
        iov[0].iov_len  = strlen(header);
        
        iov[1].iov_base = (void*)body;
        iov[1].iov_len  = strlen(body);
        
        iov[2].iov_base = (void*)footer;
        iov[2].iov_len  = strlen(footer);

        size_t expected = iov[0].iov_len + iov[1].iov_len + iov[2].iov_len;

        // CALL THE PATCHED FUNCTION
        ssize_t written = sceKernelWritev_patched(fd, iov, 3);
        
        if (written == (ssize_t)expected) {
            log_test("sceKernelWritev", true);
        } else {
            printf("Writev result: %zd, expected: %zu\n", written, expected);
            log_test("sceKernelWritev", false);
        }

        sceKernelClose(fd);
    }

    printf("\n--- Test Finished ---\n");
    
    // Prevent immediate exit so we can read the logs
    while(1) {
        sceKernelUsleep(1000000);
    }

    return 0;
}