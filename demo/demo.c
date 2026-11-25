#include <stdint.h>
#include <stdio.h>

#define LIMIT 20000000
#define DEFAULT_LIMIT 5000000

static unsigned char composite[LIMIT + 1];

static uint64_t sum_primes(uint32_t limit) {
    uint64_t total = 0;
    for (uint32_t candidate = 2; candidate <= limit; candidate++) {
        if (composite[candidate]) {
            continue;
        }
        total += candidate;
        for (uint64_t multiple = (uint64_t)candidate * candidate; multiple <= limit; multiple += candidate) {
            composite[multiple] = 1;
        }
    }
    return total;
}

static uint32_t collatz_length(uint64_t value) {
    uint32_t steps = 1;
    while (value != 1) {
        value = (value & 1) ? 3 * value + 1 : value >> 1;
        steps++;
    }
    return steps;
}

static void longest_collatz(uint32_t limit, uint32_t *best_start, uint32_t *best_length) {
    *best_start = 1;
    *best_length = 1;
    for (uint32_t start = 1; start < limit; start++) {
        uint32_t length = collatz_length(start);
        if (length > *best_length) {
            *best_length = length;
            *best_start = start;
        }
    }
}

static uint64_t mix_hash(uint32_t rounds) {
    uint64_t state = 0x9e3779b97f4a7c15ULL;
    uint64_t hash = 0xcbf29ce484222325ULL;
    for (uint32_t round = 0; round < rounds; round++) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        hash = (hash ^ (state & 0xff)) * 0x100000001b3ULL;
    }
    return hash;
}

int main(void) {
    long requested = 0;
    if (scanf("%ld", &requested) != 1 || requested < 2) {
        requested = DEFAULT_LIMIT;
    }
    if (requested > LIMIT) {
        requested = LIMIT;
    }
    uint32_t limit = (uint32_t)requested;
    uint32_t best_start = 0;
    uint32_t best_length = 0;
    longest_collatz(limit / 4, &best_start, &best_length);
    printf("primes up to %u sum to %llu\n", limit, (unsigned long long)sum_primes(limit));
    printf("longest collatz chain below %u starts at %u with %u steps\n", limit / 4, best_start, best_length);
    printf("mix hash: %016llx\n", (unsigned long long)mix_hash(limit * 4));
    return 0;
}
