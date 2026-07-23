#include <orbis/libkernel.h> 
#include <stdio.h>

int main(void) {
    sceKernelDebugOutText(0, "Hello from unemups4!\n");

    int counter = 0;
    while(counter < 5) {
        char buffer[64];
        snprintf(buffer, sizeof(buffer), "Loop counter: %d\n", counter++);
        
        sceKernelDebugOutText(0, buffer);
    }

    return 0;
}