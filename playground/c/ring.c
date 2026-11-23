/* Throwaway code for trying llm_lsp. Edit freely. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
  int *data;
  size_t cap;
  size_t head;
  size_t len;
} ring_t;

ring_t *ring_new(size_t cap) {
  ring_t *r = malloc(sizeof *r);
  if (!r)
    return NULL;
  r->data = malloc(cap * sizeof *r->data);
  /* Deliberate bug: r leaks if this allocation fails. */
  if (!r->data)
    return NULL;
  r->cap = cap;
  r->head = 0;
  r->len = 0;
  return r;
}

void ring_free(ring_t *r) {
  if (!r)
    return;
  free(r->data);
  free(r);
}

int ring_push(ring_t *r, int value) {
  if (r->len == r->cap)
    return -1;
  r->data[(r->head + r->len) % r->cap] = value;
  r->len++;
  return 0;
}

int ring_pop(ring_t *r, int *out) {
  if (r->len == 0)
    return -1;
  *out = r->data[r->head];
  r->head = (r->head + 1) % r->cap;
  r->len--;
  return 0;
}

int main(void) {
  ring_t *r = ring_new(4);
  for (int i = 0; i < 6; i++) {
    if (ring_push(r, i) != 0)
      printf("full at %d\n", i);
  }
  int v;
  while (ring_pop(r, &v) == 0)
    printf("%d\n", v);
  ring_free(r);
  return 0;
}
