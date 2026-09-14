// Independent positional-I/O oracle: no E4, Rust, CRC library, or uncached flags.
// Four isolated processes/files. Expected words derive from page/owner/version.
// Successful pwrite must be visible to subsequent pread without an fsync.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#define PAGES 16384
#define STEPS 200000
static uint64_t pattern(unsigned worker,unsigned page,uint32_t gen,unsigned word) {
    return ((uint64_t)(worker+1)<<60)^((uint64_t)page<<40)^((uint64_t)gen<<9)^word;
}
static void transfer(int fd,void *buf,off_t offset,int writing) {
    size_t done=0;
    while(done<4096) {
        ssize_t n=writing?pwrite(fd,(char*)buf+done,4096-done,offset+done):pread(fd,(char*)buf+done,4096-done,offset+done);
        if(n<0 && errno==EINTR)continue;
        if(n<=0){perror(writing?"pwrite":"pread");exit(3);}done+=(size_t)n;
    }
}
static int run(const char *root,unsigned worker) {
    char path[1024];snprintf(path,sizeof path,"%s/worker-%u.data",root,worker);
    int fd=open(path,O_RDWR|O_CREAT|O_EXCL,0600);if(fd<0){perror("open");return 3;}
    uint32_t *versions=calloc(PAGES,sizeof *versions);if(!versions)return 3;
    uint64_t *writebuf=NULL,*readbuf=NULL;
    if(posix_memalign((void**)&writebuf,4096,4096)||posix_memalign((void**)&readbuf,4096,4096))return 3;
    for(unsigned p=0;p<PAGES;p++) {
        for(unsigned w=0;w<512;w++)writebuf[w]=pattern(worker,p,0,w);
        transfer(fd,writebuf,(off_t)p*4096,1);
    }
    uint64_t random=0x9e3779b97f4a7c15ULL+worker;
    for(unsigned step=0;step<STEPS;step++) {
        random^=random<<13;random^=random>>7;random^=random<<17;
        unsigned p=(unsigned)(random%PAGES);uint32_t gen=++versions[p];
        for(unsigned w=0;w<512;w++)writebuf[w]=pattern(worker,p,gen,w);
        transfer(fd,writebuf,(off_t)p*4096,1);
        memset(writebuf,0xa5,4096); // Buffer is reusable after pwrite returns.
        // Check both the just-written page and a previously written page.
        for(unsigned check=0;check<2;check++) {
            unsigned q=check?(unsigned)((random>>32)%PAGES):p;
            transfer(fd,readbuf,(off_t)q*4096,0);
            for(unsigned w=0;w<512;w++)if(readbuf[w]!=pattern(worker,q,versions[q],w)) {
                fprintf(stderr,"MISMATCH worker=%u step=%u check=%u page=%u generation=%u word=%u want=%016" PRIx64 " got=%016" PRIx64 "\n",worker,step,check,q,versions[q],w,pattern(worker,q,versions[q],w),readbuf[w]);
                snprintf(path,sizeof path,"%s/worker-%u-observed.bin",root,worker);
                int evidence=open(path,O_WRONLY|O_CREAT|O_EXCL,0600);if(evidence<0)return 3;transfer(evidence,readbuf,0,1);close(evidence);
                for(unsigned i=0;i<512;i++)writebuf[i]=pattern(worker,q,versions[q],i);
                snprintf(path,sizeof path,"%s/worker-%u-expected.bin",root,worker);
                evidence=open(path,O_WRONLY|O_CREAT|O_EXCL,0600);if(evidence<0)return 3;transfer(evidence,writebuf,0,1);close(evidence);
                close(fd);return 1;
            }
        }
    }
    printf("PASS worker=%u writes=%u reads=%u\n",worker,PAGES+STEPS,STEPS*2);fflush(stdout);close(fd);free(versions);free(writebuf);free(readbuf);return 0;
}
int main(int argc,char **argv) {
    if(argc!=2)return 2;
    if(mkdir(argv[1],0700)){perror("mkdir fresh artifact root");return 2;}
    for(unsigned i=0;i<4;i++){pid_t p=fork();if(p<0)return 3;if(p==0)exit(run(argv[1],i));}
    int failed=0,status;while(wait(&status)>0)if(!WIFEXITED(status)||WEXITSTATUS(status))failed=1;
    return failed;
}
