// nose_shim.cc — implementation of the C seam. See nose_shim.h.
#include "nose_shim.h"

#include <stdlib.h>
#include <string.h>

#include <xapian.h>

namespace {

void set_err(char *err, size_t errlen, const std::string &m) {
    if (err && errlen) {
        strncpy(err, m.c_str(), errlen - 1);
        err[errlen - 1] = '\0';
    }
}

} // namespace

extern "C" {

void *nose_open(const char *path, char *err, size_t errlen) {
    try {
        return new Xapian::WritableDatabase(path, Xapian::DB_CREATE_OR_OPEN);
    } catch (const Xapian::Error &e) {
        set_err(err, errlen, e.get_description());
        return nullptr;
    } catch (...) {
        set_err(err, errlen, "open: unknown");
        return nullptr;
    }
}

void nose_close(void *db) { delete static_cast<Xapian::WritableDatabase *>(db); }

int nose_commit(void *db, char *err, size_t errlen) {
    try {
        static_cast<Xapian::WritableDatabase *>(db)->commit();
        return 0;
    } catch (const Xapian::Error &e) {
        set_err(err, errlen, e.get_description());
        return -1;
    } catch (...) {
        set_err(err, errlen, "commit: unknown");
        return -1;
    }
}

uint32_t nose_add_doc(void *db, const char *data, const char *title,
                       const char *text, const char *lang, char *err,
                       size_t errlen) {
    try {
        Xapian::Document doc;
        doc.set_data(data ? data : "");
        Xapian::TermGenerator tg;
        // spelling lands in an EXPLICIT database, never implicitly where
        // the document goes (without this call the spelling table stays
        // empty; with SPELLING but no db, index_text throws).
        tg.set_database(*static_cast<Xapian::WritableDatabase *>(db));
        tg.set_flags(Xapian::TermGenerator::FLAG_SPELLING);
        try {
            tg.set_stemmer(Xapian::Stem(lang ? lang : "none"));
        } catch (const Xapian::InvalidArgumentError &) {
            tg.set_stemmer(Xapian::Stem("none"));
        }
        tg.set_document(doc);
        if (title && *title) {
            tg.index_text(title);          // default scope
            tg.index_text(title, 1, "S");  // title: prefix scope
        }
        if (text && *text) {
            tg.index_text(text);
        }
        Xapian::docid id =
            static_cast<Xapian::WritableDatabase *>(db)->add_document(doc);
        return static_cast<uint32_t>(id);
    } catch (const Xapian::Error &e) {
        set_err(err, errlen, e.get_description());
        return 0;
    } catch (...) {
        set_err(err, errlen, "add_doc: unknown");
        return 0;
    }
}

int nose_add_synonym(void *db, const char *term, const char *synonym,
                     char *err, size_t errlen) {
    try {
        static_cast<Xapian::WritableDatabase *>(db)->add_synonym(term, synonym);
        return 0;
    } catch (const Xapian::Error &e) {
        set_err(err, errlen, e.get_description());
        return -1;
    } catch (...) {
        set_err(err, errlen, "synonym: unknown");
        return -1;
    }
}

uint32_t nose_doc_count(void *db) {
    try {
        return static_cast<uint32_t>(
            static_cast<Xapian::WritableDatabase *>(db)->get_doccount());
    } catch (...) {
        return 0;
    }
}

nose_results *nose_search(void *db, const char *query, const char *lang,
                          size_t limit) {
    nose_results *r =
        static_cast<nose_results *>(calloc(1, sizeof(nose_results)));
    if (!r) return nullptr;
    try {
        Xapian::WritableDatabase *wdb =
            static_cast<Xapian::WritableDatabase *>(db);
        Xapian::QueryParser qp;
        try {
            qp.set_stemmer(Xapian::Stem(lang ? lang : "none"));
        } catch (const Xapian::InvalidArgumentError &) {
            qp.set_stemmer(Xapian::Stem("none"));
        }
        qp.set_stemming_strategy(Xapian::QueryParser::STEM_SOME);
        qp.set_database(*wdb);
        unsigned flags = Xapian::QueryParser::FLAG_DEFAULT |
                         Xapian::QueryParser::FLAG_SYNONYM |
                         Xapian::QueryParser::FLAG_AUTO_SYNONYMS |
                         Xapian::QueryParser::FLAG_SPELLING_CORRECTION;
        Xapian::Query q = qp.parse_query(query ? query : "", flags);
        Xapian::Enquire enq(*wdb);
        enq.set_query(q);
        Xapian::MSet m = enq.get_mset(0, limit > 0 ? limit : 10);
        r->n = m.size();
        if (r->n) {
            r->hits = static_cast<nose_hit *>(
                calloc(r->n, sizeof(nose_hit)));
            if (!r->hits) {
                r->n = 0;
                return r;
            }
            size_t i = 0;
            for (Xapian::MSetIterator it = m.begin(); it != m.end(); ++it, ++i) {
                r->hits[i].docid = static_cast<uint32_t>(*it);
                r->hits[i].weight = it.get_weight();
                std::string url = it.get_document().get_data();
                r->hits[i].url = static_cast<char *>(malloc(url.size() + 1));
                if (r->hits[i].url) memcpy(r->hits[i].url, url.c_str(), url.size() + 1);
            }
        }
    } catch (const Xapian::Error &e) {
        set_err(r->err, sizeof(r->err), e.get_description());
    } catch (...) {
        set_err(r->err, sizeof(r->err), "search: unknown");
    }
    return r;
}

void nose_free_results(nose_results *r) {
    if (!r) return;
    for (size_t i = 0; i < r->n; ++i) free(r->hits[i].url);
    free(r->hits);
    free(r);
}

int nose_suggest(void *db, const char *word, char *out, size_t outlen) {
    if (!word || !out || !outlen) return 0;
    try {
        std::string s =
            static_cast<Xapian::WritableDatabase *>(db)->get_spelling_suggestion(
                word);
        if (s.empty() || s == word) return 0;
        strncpy(out, s.c_str(), outlen - 1);
        out[outlen - 1] = '\0';
        return 1;
    } catch (...) {
        return 0;
    }
}

} // extern "C"
