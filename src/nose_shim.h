// nose_shim.h — the magpie's nose: a minimal C seam over Xapian's C++ API.
// Xapian ships no C API, so this file IS the boundary. Every entry point
// catches Xapian exceptions and reports them through err buffers —
// exceptions never cross into Rust (that way lies UB).
// Surface kept to exactly what the spike needs: open, add, synonym,
// search, suggest, count, commit. Nothing else.

#ifndef MAGPIE_NOSE_SHIM_H
#define MAGPIE_NOSE_SHIM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Opaque writable handle (actually Xapian::WritableDatabase*).
// NULL return = failure, err buffer holds the reason.
void *nose_open(const char *path, char *err, size_t errlen);
void nose_close(void *db);
int nose_commit(void *db, char *err, size_t errlen);

// Index one page. `data` is opaque (Rust packs JSON); title is indexed
// both plain and under the S prefix; body plain. Returns docid, 0 on error.
uint32_t nose_add_doc(void *db, const char *data, const char *title,
                       const char *text, const char *lang, char *err,
                       size_t errlen);

// Arrange for a query on `term` to also match `synonym`.
// Both must be lowercase (TermGenerator lowercases; mixed case never meets).
int nose_add_synonym(void *db, const char *term, const char *synonym,
                     char *err, size_t errlen);

uint32_t nose_doc_count(void *db);

typedef struct {
    uint32_t docid;
    double weight;
    char *url; // malloc'd; freed by nose_free_results
} nose_hit;

typedef struct {
    nose_hit *hits; // malloc'd array (NULL when n == 0)
    size_t n;
    char err[256]; // empty when ok
} nose_results;

// BM25 search, best `limit` hits. lang stems the query the same way docs
// were stemmed. err[0] != 0 on failure (results may still carry hits).
nose_results *nose_search(void *db, const char *query, const char *lang,
                          size_t limit);
void nose_free_results(nose_results *r);

// Spelling suggestion for one word from the accumulated spelling table.
// Returns 1 and fills `out` when the table knows better, else 0.
int nose_suggest(void *db, const char *word, char *out, size_t outlen);

#ifdef __cplusplus
}
#endif

#endif
