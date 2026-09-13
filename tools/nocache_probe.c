// Isolate macOS F_NOCACHE roundtrips from Rust and the E4 kernel.
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
static int probe(const char *directory, int worker, int nocache) {
    int failures = 0;
    for (int aligned = 0; aligned < 2; aligned++) {
        for (int n = 0; n < 100; n++) {
            char path[4096];
            snprintf(path, sizeof(path), "%s/nocache-%d-%d-%d", directory, worker, aligned, n);
            int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
            if (fd < 0 || (nocache && fcntl(fd, F_NOCACHE, 1) < 0)) { perror("open/nocache"); return 3; }
            char *wbase, *rbase;
            if (aligned) {
                if (posix_memalign((void **)&wbase, 4096, 8192) || posix_memalign((void **)&rbase, 4096, 8192)) return 4;
            } else { wbase = calloc(1, 4096); rbase = calloc(1, 4096); }
            char *w = wbase, *r = rbase;
            if (aligned || n % 2) memset(w, n % 2 ? 0xa5 : 0, 4096);
            if (aligned) memset(r, 0x7b, 4096);
            if (pwrite(fd, w, 4096, 0) != 4096 || fsync(fd) || pread(fd, r, 4096, 0) != 4096) { perror("io"); return 5; }
            if (memcmp(w, r, 4096)) {
                printf("mismatch aligned=%d iteration=%d expected=%02x actual=%02x path=%s\n", aligned, n, (unsigned char)w[0], (unsigned char)r[0], path);
                failures++;
            }
            int same = !memcmp(w, r, 4096);
            close(fd); free(wbase); free(rbase);
            if (same) unlink(path);
        }
    }
    printf("failures=%d attempts=200\n", failures);
    return failures ? 1 : 0;
}

struct args { const char *directory; int worker; int result; int nocache; };
static void *start(void *p) { struct args *a = p; a->result = probe(a->directory, a->worker, a->nocache); return NULL; }
int main(int argc, char **argv) {
    if (argc != 3 || (strcmp(argv[2], "nocache") && strcmp(argv[2], "buffered"))) return 2;
    pthread_t threads[8]; struct args args[8]; int failures = 0;
    for (int i=0; i<8; i++) { args[i] = (struct args){argv[1], i, 0, !strcmp(argv[2], "nocache")}; pthread_create(&threads[i], NULL, start, &args[i]); }
    for (int i=0; i<8; i++) { pthread_join(threads[i], NULL); failures += args[i].result; }
    return failures ? 1 : 0;
}
