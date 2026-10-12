"""Resource lifetimes across tasks, FIFOs and lifecycle events: objects that
live and die while work that names them is queued, running or held on
another FIFO; objects made again under the same reference with other
contents or another type; heaps whose buffers and textures alias each
other; resources paged across many execs while the guest changes the RAM
behind them; and what survives a pause and a suspend
(docs/testing.md, sections 4.10, 6.10-6.12, 6.19, 6.20, 6.24).

pgtest/lifetime_util.py restates the layouts and says what the scenarios
lean on. Unless a docstring says otherwise a scenario runs in all three
setups, and where they differ (section 6.10's table: in-process heaps and
private buffers are host copies paged by blits, the service's are detach
resources over the guest pages) the difference is locked.
"""
import struct

from pgtest import abi
from pgtest.abi import PAGE
from pgtest.lifetime_util import (B, C, IG, IH, SY, Lab, Life, blit, delete_object, delete_resource, either,
                                  invalidate_payload, pattern, refs_payload, rewrite_entry, run_either, tex2d,
                                  tex_fill, tex_in, tex_out, texels, view)
from pgtest.registry import scenario
from pgtest.resources_build import buffer_rec, heap_buffer_rec, heap_rec, ser_in_heap, desc1
from pgtest.scenario import Scenario
from pgtest.task import seg

H4 = 4 * PAGE


def _scn(fn, suffix="", **kw):
    return Scenario(fn.__name__ + suffix, doc=fn.__doc__, **kw)


# ---- heaps: aliasing and lifetimes ----

@scenario
def interplay_lifetime_heap_alias_buffers():
    """Two buffers placed in one private heap over the same range (types 13
    and 14: A at offset 0 and B at offset 0x4000, each two pages, so A's
    second page is B's first). Both read the guest bytes the heap was paged
    from; a fill through A shows through B in the next exec, and a fill
    through B through A. In-process the heap is a host copy (a legacy
    placement heap paged by one blit): the guest pages keep their bytes
    until an exec lists the heap with invalidateGuest and synchronize (6),
    which writes both fills back. In the service the heap is a detach heap
    over the guest pages: each fill is in the guest pages at once."""
    s = _scn(interplay_lifetime_heap_alias_buffers)
    s.bring_up()
    t = Lab(s, pages=80)
    hva = t.reserve(H4)
    t.write(hva, pattern(H4, 5, 1))
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, H4, 1))
    a = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, 2 * PAGE))
    b = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, PAGE, 2 * PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    res = [(h, 0), (a, 0), (b, 0), (out, 0)]
    t.run(blit(B.copy_buffer_to_buffer(a, out, PAGE, 0, 0x100), B.copy_buffer_to_buffer(b, out, 0, 0x100, 0x100)),
          resources=res, label="A's second page and B's first, copied out")
    s.cmp(t.gpa(ova), t.gpa(ova + 0x100), 0x100, label="A+0x4000 == B+0")
    s.cmp(t.gpa(ova), t.gpa(hva + PAGE), 0x100, label="both == the guest's heap bytes at 0x4000")
    t.run(blit(B.fill(a, PAGE + 0x40, 0x40, 0xA5)), resources=res, label="fill through A")
    t.run(blit(B.copy_buffer_to_buffer(b, out, 0, 0x200, 0x100)), resources=res, label="read through B")
    t.run(blit(B.fill(b, 0x80, 0x20, 0x5A)), resources=res, label="fill through B")
    t.run(blit(B.copy_buffer_to_buffer(a, out, PAGE, 0x300, 0x100)), resources=res, label="read through A")
    t.dump(ova + 0x200, 0x200, "read through B, then through A")
    t.dump(hva + PAGE, 0x100, "the heap's guest pages at 0x4000, before a synchronize")
    t.run(blit(B.fill(out, 0xF00, 4, 0)), resources=[(h, IG | SY), (out, 0)], label="the heap listed with flags 6")
    s.cmp(t.gpa(hva + PAGE), t.gpa(ova + 0x300), 0x100, label="guest heap pages at 0x4000 == A's last read")
    t.dump(hva, 0x40, "the heap's guest pages at 0, untouched")
    t.delete()
    s.events("end")
    return s


@scenario
def interplay_lifetime_heap_alias_texture():
    """A 4x4 RGBA8Unorm texture placed at offset 0 of a two-page private
    heap (type 8, commandType 21, made with the heap's options) and a heap
    buffer over the same bytes (type 14 at offset 0). The texture reads the
    bytes the heap was paged from; a fill of the buffer shows in the
    texture, and a colour fill of the texture (321) shows in the buffer,
    each in the next exec. Every value is one byte repeated, so what is read
    does not depend on how the GPU lays the texels out (a texture in a heap
    is not linear on this GPU). Only the heap is listed: a serializer
    texture listed faults."""
    s = _scn(interplay_lifetime_heap_alias_texture)
    s.bring_up()
    t = Lab(s, pages=80)
    hva = t.reserve(2 * PAGE)
    t.write(hva, b"\x6B" * 0x40 + pattern(0x100, 3, 9))
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, 2 * PAGE, 1))
    tex = t.add(abi.T_SERIALIZER_TEXTURE, ser_in_heap(h, 0, desc1(usage=3)))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    res = [(h, 0), (hb, 0), (out, 0)]
    t.run(blit(tex_out(tex, out, 0), B.copy_buffer_to_buffer(hb, out, 0, 0x40, 0x40)), resources=res,
          label="the texture and the buffer read")
    s.cmp(t.gpa(ova), t.gpa(ova + 0x40), 0x40, label="texture == buffer")
    t.run(blit(B.fill(hb, 0, 0x40, 0x3C)), resources=res, label="fill the buffer")
    t.run(blit(tex_out(tex, out, 0x80)), resources=res, label="read the texture")
    t.run(blit(tex_fill(tex, (1.0, 0.0, 0.0, 1.0))), resources=res, label="fill the texture red")
    t.run(blit(B.copy_buffer_to_buffer(hb, out, 0, 0xC0, 0x40)), resources=res, label="read the buffer")
    t.dump(ova, 0x100, "reads")
    t.delete()
    s.events("end")
    return s


@scenario
def interplay_lifetime_heap_deleted_under_buffer():
    """A heap deleted (37) while a buffer placed in it lives: the heap
    buffer keeps the old heap's Metal heap (the heap buffer takes only its
    MTLHeap), and the next exec that lists the heap makes a new one from the
    record. In-process the new heap is paged from the guest pages, which
    never saw the old heap buffer's fill: a new heap buffer placed at the
    same offset reads the guest bytes while the old one still reads its
    fill, and synchronizing the new heap (flags 6) writes the guest's own
    bytes back, so the old buffer's later fill never reaches the guest. In
    the service both heaps are detach heaps over the same guest pages, so
    both buffers see one memory and every fill is in the guest pages."""
    s = _scn(interplay_lifetime_heap_deleted_under_buffer)
    s.bring_up()
    t = Lab(s, pages=80)
    hva = t.reserve(H4)
    t.write(hva, pattern(H4, 7, 2))
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, H4, 1))
    old = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, PAGE, PAGE))
    new = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, PAGE, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    t.run(blit(B.fill(old, 0, 0x40, 0xA1)), resources=[(h, 0), (old, 0), (out, 0)], label="fill the old buffer")
    s.submit(*delete_resource(1, h), label="37: the heap")
    t.run(blit(B.copy_buffer_to_buffer(old, out, 0, 0, 0x80)), resources=[(h, 0), (old, 0), (out, 0)],
          label="the heap made again; the old buffer read")
    t.run(blit(B.copy_buffer_to_buffer(new, out, 0, 0x80, 0x80)), resources=[(h, 0), (new, 0), (out, 0)],
          label="a buffer made in the new heap, read")
    t.run(blit(B.fill(old, 0x40, 0x40, 0xB2)), resources=[(h, 0), (old, 0), (out, 0)],
          label="fill the old buffer again")
    t.run(blit(B.copy_buffer_to_buffer(new, out, 0, 0x100, 0x80)), resources=[(h, 0), (new, 0), (out, 0)],
          label="the new buffer read")
    t.dump(ova, 0x180, "old, new, new after the old's second fill")
    t.run(blit(B.fill(out, 0xF00, 4, 0)), resources=[(h, IG | SY), (out, 0)], label="the new heap, flags 6")
    t.dump(hva + PAGE, 0x80, "the heap's guest pages at 0x4000")
    s.submit(*delete_resource(1, old), label="37: the old buffer")
    t.run(blit(B.copy_buffer_to_buffer(old, out, 0, 0x180, 0x80)), resources=[(h, 0), (old, 0), (out, 0)],
          label="the old buffer's reference made again, in the new heap")
    s.cmp(t.gpa(ova + 0x180), t.gpa(hva + PAGE), 0x80, label="== the heap's guest pages")
    t.delete()
    s.events("end")
    return s


@scenario
def interplay_lifetime_heap_discard_relist():
    """A private heap across execs while the guest rewrites its pages
    between them. In-process a listed heap is paged once: the guest's new
    bytes stay unseen by an exec with flags 0; DiscardResources (63) makes
    the heap volatile and drops its transfer buffer, and the next prepare
    makes it non-volatile again and finds it host-valid, so it is not
    paged either; only invalidateHost (1) pages the new bytes in. In the
    service the heap is the guest pages: every read shows the latest
    bytes, and 63 detaches the backing, which the next listing attaches
    again."""
    s = _scn(interplay_lifetime_heap_discard_relist)
    s.bring_up()
    t = Lab(s, pages=80)
    hva = t.reserve(H4)
    t.write(hva, pattern(H4, 3, 0x10))
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, H4, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 2 * PAGE, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()

    def read(k, flags, label):
        t.run(blit(B.copy_buffer_to_buffer(hb, out, 0, 0x40 * k, 0x40)), resources=[(h, flags), (hb, 0), (out, 0)],
              label=label)

    read(0, 0, "first read")
    t.write(hva + 2 * PAGE, b"\x11" * 0x40)
    read(1, 0, "the guest wrote 0x11s; flags 0")
    s.submit(abi.OP_DISCARD_RESOURCES, refs_payload(1, [h]), label="63: the heap")
    read(2, 0, "after 63, flags 0")
    t.write(hva + 2 * PAGE, b"\x22" * 0x40)
    read(3, IH, "the guest wrote 0x22s; flags 1")
    t.write(hva + 2 * PAGE, b"\x33" * 0x40)
    read(4, 0, "the guest wrote 0x33s; flags 0")
    t.dump(ova, 0x140, "five reads")
    t.delete()
    s.events("end")
    return s


# ---- buffers made again under the same reference ----

@scenario
def interplay_lifetime_buffer_record_rewritten():
    """The guest rewrites a buffer's object-list entry and record in place
    (no 51: the list is read in place). A live buffer ignores it: its object
    was made from the old record and is cached. After DeleteResource (37)
    the next exec makes it from the new record: another page, another
    length, a private buffer where there was a shared one and back. Each
    read is a blit copy into a shared result buffer."""
    s = _scn(interplay_lifetime_buffer_record_rewritten)
    s.bring_up()
    t = Lab(s, pages=80)
    x, xva = t.shared(contents=b"\xA0" * PAGE, listed=False)
    yva = t.reserve(2 * PAGE)
    t.write(yva, b"\xB0" * PAGE + b"\xB1" * PAGE)
    zva = t.reserve(PAGE)
    t.write(zva, b"\xC0" * PAGE)
    out, ova = t.shared(listed=False)
    t.ready()

    def read(k, label, n=0x40, src_off=0):
        t.run(blit(B.copy_buffer_to_buffer(x, out, src_off, 0x40 * k, n)), resources=[(x, 0), (out, 0)], label=label)

    read(0, "the buffer over page X")
    rewrite_entry(t, x, abi.T_BUFFER, buffer_rec(2 * PAGE, yva // PAGE))
    read(1, "entry rewritten to page Y, live object")
    s.submit(*delete_resource(1, x), label="37")
    read(2, "made again: two pages at Y", src_off=PAGE)
    rewrite_entry(t, x, abi.T_BUFFER, buffer_rec(PAGE, zva // PAGE, flags=1))
    s.submit(*delete_resource(1, x), label="37 again")
    read(3, "made again: private at Z")
    t.run(blit(B.fill(x, 0, 0x10, 0xC7)), resources=[(x, 0), (out, 0)], label="fill the private buffer")
    t.dump(zva, 0x20, "Z's pages after the fill, flags 0")
    s.submit(*delete_object(1, 1000, x), label="40, operation 1000")
    rewrite_entry(t, x, abi.T_BUFFER, buffer_rec(PAGE, zva // PAGE))
    read(4, "made again: shared at Z")
    t.dump(ova, 0x140, "five reads")
    t.delete()
    s.events("end")
    return s


@scenario
def interplay_lifetime_buffer_retyped():
    """A reference whose entry the guest retypes between execs, each time
    after a delete (37) so that the next exec makes it again: a private heap
    (type 13), which in-process has no Resource::get_buffer (its kind raises
    Unrecognized, a fault) and in the service is its cover buffer over the
    heap's pages; a normal texture (type 2), which is no buffer in any setup;
    type 0 ("Attempting to fetch invalid object"); and a buffer again, which
    works. A retyped entry whose object is live changes nothing."""
    s = _scn(interplay_lifetime_buffer_retyped)
    s.bring_up()
    t = Lab(s, pages=80)
    x, xva = t.shared(contents=b"\xA0" * PAGE, listed=False)
    hva = t.reserve(PAGE)
    t.write(hva, b"\xD0" * PAGE)
    out, ova = t.shared(listed=False)
    t.ready()
    heap = heap_rec(hva // PAGE, PAGE, 1)
    from pgtest.resources_build import simple_dim, texture_rec
    tex = texture_rec(PAGE, hva // PAGE, [simple_dim(4, 4)], desc1())

    def read(k, label):
        run_either(t, blit(B.copy_buffer_to_buffer(x, out, 0, 0x40 * k, 0x40)), [(x, 0), (out, 0)], label)

    read(0, "a buffer")
    rewrite_entry(t, x, abi.T_HEAP, heap)
    read(1, "retyped as a heap, the buffer live")
    s.submit(*delete_resource(1, x), label="37")
    read(2, "a heap made under the reference")
    rewrite_entry(t, x, abi.T_TEXTURE, tex)
    s.submit(*delete_resource(1, x), label="37")
    read(3, "a texture made under the reference")
    rewrite_entry(t, x, 0, va=0, length=0)
    s.submit(*delete_resource(1, x), label="37")
    read(4, "type 0")
    rewrite_entry(t, x, abi.T_BUFFER, buffer_rec(PAGE, xva // PAGE))
    read(5, "a buffer again")
    t.dump(ova, 0x180, "reads")
    t.delete()
    s.events("end")
    return s


# ---- objects deleted while an exec that names them is held ----

def _held_heap(which):
    s = Scenario("interplay_lifetime_held_heap_deleted_" + which, doc=interplay_lifetime_held_heap_deleted.__doc__)
    g = Life(s)
    t = g.t
    hva = t.reserve(H4)
    t.write(hva, pattern(H4, 9, 4))
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, H4, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, PAGE, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    res = [(h, 0), (hb, 0), (out, 0)]
    t.run(blit(B.fill(hb, 0, 0x40, 0xA1)), resources=res, label="fill the heap buffer")
    st = g.hold(blit(B.copy_buffer_to_buffer(hb, out, 0, 0, 0x80)), res, "copy the heap buffer out")
    victim = h if which == "heap" else hb
    g.work(*delete_resource(1, victim), label="37 on the worker: the " + which.replace("_", " "))
    s.events("deleted while held")
    g.release(st, "released")
    t.run(blit(B.copy_buffer_to_buffer(hb, out, 0, 0x80, 0x80)), resources=res, label="again")
    t.dump(ova, 0x100, "the held copy, then another")
    t.dump(hva + PAGE, 0x80, "the heap's guest pages at 0x4000")
    s.events("end")
    return s


@scenario
def interplay_lifetime_held_heap_deleted():
    """An exec that copies a heap buffer out waits on the gate while the
    worker FIFO (child 2) deletes (37) the heap, or the heap buffer. The
    heap buffer was filled with 0xa1s before. Released, the exec lists the
    heap and the buffer, so what was deleted is made again from its record:
    a new heap is paged from the guest pages, but the live heap buffer keeps
    the old heap and its fill; a new heap buffer is placed in the live heap
    at the same offset and reads the fill too. In-process the fill never
    reaches the guest pages; in the service it is in them at once."""
    return [_held_heap("heap"), _held_heap("heap_buffer")]


def _held_texture(synced):
    s = Scenario("interplay_lifetime_held_texture_deleted" + ("_synced" if synced else ""),
                 doc=interplay_lifetime_held_texture_deleted.__doc__)
    g = Life(s)
    t = g.t
    tex, tva = tex2d(t, contents=texels(4, 4))
    out, ova = t.shared(listed=False)
    t.ready()
    t.run(blit(tex_fill(tex, (0.0, 0.0, 1.0, 1.0))), resources=[(tex, (IG | SY) if synced else 0), (out, 0)],
          label="fill the texture blue" + (", flags 6" if synced else ""))
    t.run(blit(tex_out(tex, out, 0)), resources=[(tex, 0), (out, 0)], label="read it")
    st = g.hold(blit(tex_out(tex, out, 0x40)), [(tex, 0), (out, 0)], "read it again")
    g.work(*delete_resource(1, tex), label="37 on the worker: the texture")
    g.release(st, "released")
    t.dump(ova, 0x80, "before the delete, and the held read")
    t.dump(tva, 0x40, "the texture's guest page")
    s.events("end")
    return s


@scenario
def interplay_lifetime_held_texture_deleted():
    """A normal texture (type 2, private) over guest texels, filled blue by
    the GPU, then read; a second read waits on the gate while the worker
    deletes (37) the texture. Released, the texture is made again from its
    guest page, so the read shows the guest's texels: the fill is lost,
    unless the fill's exec listed the texture with invalidateGuest and
    synchronize (6), which wrote it back to the page and so into the new
    texture. The same in every setup: textures are host copies in the
    service too."""
    return [_held_texture(False), _held_texture(True)]


def _held_pipeline(what):
    s = Scenario("interplay_lifetime_held_pipeline_" + what, doc=interplay_lifetime_held_pipeline.__doc__)
    g = Life(s)
    t = g.t
    from pgtest.diff_util import K_DOUBLE
    fn_before = len(t.ol.entries)
    pso = t.kernel(K_DOUBLE)
    fn = fn_before  # kernel() adds the function, then the pipeline
    out, ova = t.shared(listed=False)
    t.ready()

    def dispatch(off):
        return [seg(abi.ENC_COMPUTE,
                    C.set_pipeline(pso) + C.set_buffers(0, [(out, off)]) + C.dispatch_threads((16, 1, 1), (16, 1, 1)))]

    t.run(dispatch(0), resources=[(out, 0)], label="double_gid")
    st = g.hold(dispatch(0x40), [(out, 0)], "double_gid again")
    if what == "deleted":
        g.work(*delete_object(1, 1006, pso), label="40, operation 1006: the pipeline")
    elif what == "function_deleted":
        g.work(*delete_object(1, 1005, fn), label="40, operation 1005: its function")
    elif what == "retyped":
        g.work(*delete_object(1, 1006, pso), label="40, operation 1006: the pipeline")
        rewrite_entry(t, pso, 0, va=0, length=0)
    else:
        g.work(*delete_object(1, 1005, fn), label="40, operation 1005: its function")
        g.work(*delete_object(1, 1006, pso), label="40, operation 1006: the pipeline")
        rewrite_entry(t, fn, 0, va=0, length=0)
    g.release(st, "released")
    t.dump(ova, 0x80, "the first dispatch, and the held one")
    s.events("end")
    return s


@scenario
def interplay_lifetime_held_pipeline():
    """A compute pipeline (double_gid) dispatched, then dispatched again by
    an exec held on the gate while the worker deletes objects with
    DeleteObject (40): the pipeline (operation 1006), which the released
    exec makes again from its record and runs; its function (1005) with the
    pipeline live, which changes nothing; the pipeline with its entry then
    retyped 0, so the release faults ("Failed to create
    ComputePipelineState"); and both, with the function's entry retyped 0:
    making the pipeline again looks its function up from the empty entry,
    which raises the function lookup's failure, so the release faults
    ("function lookup failed") in every setup, where a nil function would
    have Metal abort (docs/testing.md, section 7.4)."""
    return [_held_pipeline(w) for w in ("deleted", "function_deleted", "retyped", "function_retyped")]


@scenario
def interplay_lifetime_emptied_resource():
    """A buffer whose entry the guest has emptied (type 0, offset all ones),
    as a guest empties a task's entries when the process that owns it ends,
    named by the compute lists that look resources up untyped, each before a
    dispatch of the pipeline a first dispatch made: UseResources (0x87) and
    MemoryBarrierWithResources (0xd6) in a serial encoder take it as nil,
    which Metal skips, and their dispatches run; UseResources in a concurrent
    encoder (0xdb 1), where Metal ends the process at a nil entry
    (interplay_decodewalk_26), raises instead ("resource lookup failed"), and
    the exec faults before its dispatch, in every setup. The buffer is never
    listed, so the exec's own resource list does not fault first."""
    from pgtest.diff_util import K_DOUBLE
    s = Scenario("interplay_lifetime_emptied_resource", doc=interplay_lifetime_emptied_resource.__doc__)
    s.bring_up()
    t = Lab(s)
    pso = t.kernel(K_DOUBLE)
    out, ova = t.shared()
    gone, _ = t.shared(listed=False)
    t.ready()

    def dispatch(off, records=b"", concurrent=False):
        prefix = C.dispatch_type(1) if concurrent else b""
        return [seg(abi.ENC_COMPUTE, prefix + C.set_pipeline(pso) + C.set_buffers(0, [(out, off)]) + records
                    + C.dispatch_threads((16, 1, 1), (16, 1, 1)))]

    t.run(dispatch(0), label="double_gid")
    rewrite_entry(t, gone, 0, va=u64_max(), length=0)
    t.run(dispatch(0x40, C.use_resources([out, gone])), label="0x87 [out, emptied], serial")
    t.run(dispatch(0x80, C.barrier_resources([gone])), label="0xd6 [emptied], serial")
    t.run(dispatch(0xc0, C.use_resources([out, gone]), concurrent=True), fault=True,
          label="0x87 [out, emptied], concurrent")
    t.dump(ova, 0x100, "three dispatches of four")
    s.events("end")
    return s


@scenario
def interplay_lifetime_emptied_resource_render():
    """A buffer whose entry the guest has emptied (type 0, offset all ones),
    as a guest empties a task's entries when the process that owns it ends,
    named before a triangle by the render lists that look resources up
    untyped, UseResources (0x87) and UseResourcesWithStages (0x89): nil to
    each, which Metal skips, and the triangle draws as without it, in every
    setup. So does a ref past the object list, nil to 0x89 (measured: Metal
    skips a nil entry of useResources:count:usage:stages: as it does one of
    useResources:count:usage:). The buffer is never listed, so the exec's own
    resource list does not fault first."""
    from scenarios.render_state import TRI, StateTask, case
    from pgtest import render_lib as R
    s = Scenario("interplay_lifetime_emptied_resource_render",
                 doc=interplay_lifetime_emptied_resource_render.__doc__)
    s.bring_up()
    st = StateTask(s)
    gone, _ = st.t.buffer()
    st.t.bufs.remove(gone)
    st.start()
    p = R.pipeline_state
    tri = [p(st.p_vbuf), st.vb(), R.draw16(TRI, 0, 3)]
    case(s, st, "the triangle", tri)
    rewrite_entry(st.t, gone, 0, va=u64_max(), length=0)
    case(s, st, "0x87 [pos, emptied], then the triangle", [R.use_resources([st.pos, gone], 1)] + tri)
    case(s, st, "0x89 [pos, emptied], then the triangle", [R.use_resources_stages([st.pos, gone], 1, 1)] + tri)
    past = st.t.ol.length() // 12
    case(s, st, "0x89 [pos, past the list], then the triangle",
         [R.use_resources_stages([st.pos, past], 1, 1)] + tri)
    st.t.delete()
    s.events("end")
    return s


def u64_max():
    return (1 << 64) - 1


@scenario
def interplay_lifetime_held_task_redefined():
    """Task 1 is deleted (32) and defined again (56) over another task root
    on the worker FIFO while an exec of task 1 waits on the gate on the root
    FIFO. The exec names only a task ID, task VAs and references: released,
    it runs on the new task, reading the new task's command buffer at the
    same VA (the same blit, copy reference 1 to reference 2) and its objects,
    whose list the new root's fields name. So the new task's result buffer
    gets its own source's bytes, and the old task's result buffer keeps what
    the old task wrote before."""
    s = _scn(interplay_lifetime_held_task_redefined)
    g = Life(s)
    old = g.t
    src, sva = old.shared(contents=b"\x11" * PAGE, listed=False)
    out, ova = old.shared(listed=False)
    old.ready()
    work = blit(B.copy_buffer_to_buffer(src, out, 0, 0, 0x40))
    old.run(work, resources=[(src, 0), (out, 0)], label="the old task's copy")
    ranges = old.place(work)
    st = g.hold(None, [(src, 0), (out, 0)], "the copy again", ranges=ranges)
    new = Lab(s, tid=1)
    nsrc, _ = new.shared(contents=b"\x22" * PAGE, listed=False)
    nout, nova = new.shared(listed=False)
    assert (nsrc, nout) == (src, out)
    new.write(ranges[0][0], work[0])  # at the held exec's command-buffer VA
    new.write_root(obj_off=0, obj_len=new.ol.write())
    g.work(abi.OP_DELETE_TASK, struct.pack("<I", 1), label="32 on the worker")
    new.define(fifo=g.worker, label="56 on the worker: another root")
    s.dump(s.pa(s.root_page, 4), 4, "root word of task 1")
    g.release(st, "released")
    old.dump(ova, 0x40, "the old task's result")
    new.dump(nova, 0x40, "the new task's result")
    s.events("end")
    return s


@scenario
def interplay_lifetime_define_over_live_task():
    """DefineTask2 (56) over a live task is refused ("Host task already
    exists"), and the task and its objects carry on: a private buffer the
    GPU filled still holds the fill. Task 2 defined over task 1's root page
    shares its object list (the host wrote the list's offset and length
    into the root at 51) and makes objects of its own from the same
    records: in-process its private buffer is paged from the guest pages,
    which never saw task 1's fill; in the service the buffer is the guest
    pages, which did. Task 1 deleted and defined again over its root is the
    same: a new task, whose objects are made again."""
    s = _scn(interplay_lifetime_define_over_live_task)
    s.bring_up()
    t = Lab(s)
    p, pva = t.shared(contents=b"\x77" * PAGE, flags=1, listed=False)
    out, ova = t.shared(listed=False)
    t.ready()
    t.run(blit(B.fill(p, 0, 0x40, 0x5A)), resources=[(p, 0), (out, 0)], label="fill the private buffer")
    work = blit(B.copy_buffer_to_buffer(p, out, 0, 0, 0x40))
    cbs = t.place(work)
    from pgtest.diff_util import exec_payload
    from pgtest.tasks_lib import define_raw

    def copy(tid, k, label):
        s.submit(abi.OP_EXEC_INDIRECT3, exec_payload(tid, [(cbs[0][0], cbs[0][1])], [(p, 0), (out, 0)]),
                 label=label)
        t.dump(ova, 0x40, label + ": the copy")

    copy(1, 0, "task 1")
    define_raw(s, 1 << 1, t.length, s.pfn(t.root), fault=True, label="56 over task 1")
    s.fault_state("56 over task 1")
    s.resume_root()
    s.fifo.wait_stamp(s.fifo.stamp)
    copy(1, 1, "task 1 after the refused define")
    define_raw(s, 2 << 1, t.length, s.pfn(t.root), label="56: task 2 over task 1's root")
    s.dump(s.pa(s.root_page, 4), 8, "root words of tasks 1 and 2")
    copy(2, 2, "task 2")
    s.submit(abi.OP_DELETE_TASK, struct.pack("<I", 1), label="32: task 1")
    define_raw(s, 1 << 1, t.length, s.pfn(t.root), label="56: task 1 again")
    copy(1, 3, "task 1 again")
    t.dump(pva, 0x40, "the private buffer's pages")
    s.events("end")
    return s


# ---- paging across many execs while the guest changes the RAM ----

P_FLAGS = [0, IH, IG, 0, SY, IH | IG | SY, 0]


def _paging(name, doc, opcode):
    s = Scenario(name, doc=doc)
    s.bring_up()
    t = Lab(s, pages=80)
    sh, shva = t.shared(contents=b"\xA0" * PAGE, listed=False)
    p, pva = t.shared(contents=b"\xB0" * PAGE, flags=1, listed=False)
    out, ova = t.shared(listed=False)
    t.ready()
    for k, f in enumerate(P_FLAGS):
        if k:
            t.write(shva, bytes([0xA0 + k]) * 0x40)
            t.write(pva, bytes([0xB0 + k]) * 0x40)
        pf = f if opcode == abi.OP_EXEC_INDIRECT3 else (p, f & 1, (f >> 1) & 1, (f >> 2) & 1)
        res = [(sh, 0), (p, f) if opcode == abi.OP_EXEC_INDIRECT3 else pf, (out, 0)]
        if opcode != abi.OP_EXEC_INDIRECT3:
            res = [(sh, 0, 0, 0), pf, (out, 0, 0, 0)]
            if k == 3:
                res[1] = (p, 2, 2, 2)  # each byte cut to its low bit: nothing
        t.run(blit(B.copy_buffer_to_buffer(sh, out, 0, 0x80 * k, 0x40), B.copy_buffer_to_buffer(p, out, 0, 0x80 * k + 0x40, 0x40),
                   B.fill(p, 0x100 + 0x10 * k, 0x10, 0xE0 + k)), resources=res, opcode=opcode,
              label="exec %d, the private buffer's flags %d" % (k, f))
        t.dump(pva, 0x40, "exec %d: the private buffer's first guest bytes" % k)
    t.dump(ova, 0x80 * len(P_FLAGS), "each exec's reads of the shared and the private buffer")
    t.dump(pva + 0x100, 0x80, "the private buffer's filled guest bytes")
    s.submit(abi.OP_SYNCHRONIZE_RESOURCES, refs_payload(1, [p]), label="53")
    t.dump(pva + 0x100, 0x80, "after 53")
    s.events("end")
    return s


@scenario
def interplay_lifetime_paging_flags():
    """A shared and a private buffer read by seven execs in a row while the
    guest rewrites the first bytes of both before each; each exec also fills
    16 bytes of the private buffer. The private buffer is listed with flags
    0, 1, 2, 0, 4, 7, 0. In-process: it is paged at first use and at each
    invalidateHost, and otherwise keeps its host copy whatever the guest
    writes; invalidateGuest marks the guest's copy stale, and synchronize
    writes the host copy back after the exec, over what the guest wrote
    since, while invalidateHost first makes the guest's copy the valid one.
    A synchronize leaves the guest's copy valid, so exec 6's fill (flags 0)
    stays on the host and a last 53 writes nothing. The shared buffer is the
    guest pages throughout. In the service both buffers are the guest pages,
    so every read shows the guest's latest bytes and every fill is in the
    guest at once."""
    return _paging("interplay_lifetime_paging_flags", interplay_lifetime_paging_flags.__doc__,
                   abi.OP_EXEC_INDIRECT3)


@scenario
def interplay_lifetime_paging_flags_exec2():
    """interplay_lifetime_paging_flags through ExecIndirect2 (55), whose
    resource records carry one byte a flag: the same sequence gives the
    same bytes, except exec 3, which lists the private buffer with bytes
    {2, 2, 2}; each is cut to its low bit, so the listing means flags 0,
    as the sequence's own flags there."""
    return _paging("interplay_lifetime_paging_flags_exec2", interplay_lifetime_paging_flags_exec2.__doc__,
                   abi.OP_EXEC_INDIRECT2)


# ---- indirect command buffers whose references change under them ----

def _icb_exec(w, icb, label, flags=0, res_extra=(), use=None, n=1):
    """Executes n commands of a compute ICB from a compute pass (0xe4) with
    a useResources (0x87) of `use` (default the result buffer), its outcome
    whichever the setup gives (either())."""
    from scenarios.icb import exec_ranged, use_resources
    body = use_resources(*(use if use is not None else [w.out])) + exec_ranged(icb, 0, n)
    t = w.t
    t.reset_cmdbufs()
    cbs = [t.cmdbuf(seg(abi.ENC_COMPUTE, body))]
    from pgtest.diff_util import exec_payload
    res = [(w.out, 0), (w.tb, 0), (w.rng, 0), (icb, flags)] + list(res_extra)
    return either(t.s, abi.OP_EXEC_INDIRECT3, exec_payload(t.tid, cbs, res), label)


@scenario(exclusive=True)
def interplay_lifetime_icb_bound_buffer_deleted():
    """A use after delete that does not fault. A compute ICB bound to
    buffer 2 by reference is prepared and run: its Metal command holds the
    buffer's GPU address. The guest zeroes buffer 2's pages and deletes it
    (37), which forgets the object; the next exec makes buffer 2 again
    (listed, and in a useResources) but runs the ICB as prepared (flags
    0), so its dispatch writes through the deleted buffer's address. In
    every setup the writes land in buffer 2's guest pages, as if nothing
    were deleted, and nothing faults (checked in the system log while
    blessing): the old buffer wrapped the same pages. The ICB listed with
    invalidateHost is prepared again and binds the new buffer: the same
    bytes. Exclusive: a GPU write through a deleted resource's address is
    what faults a GPU when it does not land."""
    from scenarios.icb import ICBTask
    from pgtest.icb_cmds import compute_cmd
    s = Scenario("interplay_lifetime_icb_bound_buffer_deleted", exclusive=True,
                 doc=interplay_lifetime_icb_bound_buffer_deleted.__doc__)
    s.bring_up()
    w = ICBTask(s)
    o2, o2va = w.t.buffer(PAGE)
    icb = w.icb([compute_cmd(pso=w.cpso, binds=[(o2, 0, 0)], grid=(16, 1, 1), tg=(16, 1, 1))])
    w.start()
    _icb_exec(w, icb, "the ICB, bound to buffer 2", res_extra=[(o2, 0)], use=[o2])
    w.t.dump(o2va, 0x40, "buffer 2")
    w.t.write(o2va, bytes(0x40))
    s.submit(*delete_resource(1, o2), label="37: buffer 2")
    _icb_exec(w, icb, "the ICB again, buffer 2 made again", res_extra=[(o2, 0)], use=[o2])
    w.t.dump(o2va, 0x40, "buffer 2 after")
    _icb_exec(w, icb, "the ICB invalidated", flags=IH, res_extra=[(o2, 0)], use=[o2])
    w.t.dump(o2va, 0x40, "buffer 2 after the invalidated ICB")
    s.events("end")
    return s


@scenario
def interplay_lifetime_icb_recreated():
    """A compute ICB (type 20) prepared from its guest commands at its first
    exec ignores its guest memory after that: the guest rewrites its command
    (another offset and size) and the next exec runs the old one.
    DeleteResource (37) forgets it, and the next exec makes it again from the
    guest memory: the new command runs. Retyped to a buffer (type 1) after a
    delete, the execute finds no ICB (a buffer has no
    Resource::get_indirect_command_buffer: its kind raises Unrecognized) and
    faults; the
    buffer object that exec made stays, so the entry made an ICB again faults
    too, until a delete lets the next exec make an ICB, which runs."""
    from scenarios.icb import ICBTask
    from pgtest.icb_cmds import CMD, compute_cmd
    s = _scn(interplay_lifetime_icb_recreated)
    s.bring_up()
    w = ICBTask(s)
    O = w.out
    icb = w.icb([compute_cmd(pso=w.cpso, binds=[(O, 0, 0)], grid=(4, 1, 1), tg=(4, 1, 1))])
    va, size = w.icbs[icb]
    typ, rlen, rva = w.t.ol.entries[icb]
    w.start()
    _icb_exec(w, icb, "the ICB")
    w.out_dump("four u32s at 0", 0x80)
    w.clear_out()
    w.t.write(va, compute_cmd(pso=w.cpso, binds=[(O, 0, 0x40)], grid=(8, 1, 1), tg=(8, 1, 1)))
    _icb_exec(w, icb, "its guest command rewritten")
    w.out_dump("the old command ran", 0x80)
    w.clear_out()
    s.submit(*delete_resource(1, icb), label="37: the ICB")
    _icb_exec(w, icb, "made again")
    w.out_dump("the new command ran", 0x80)
    w.clear_out()
    s.submit(*delete_resource(1, icb), label="37: the ICB")
    rewrite_entry(w.t, icb, abi.T_BUFFER, buffer_rec(PAGE, va // PAGE))
    _icb_exec(w, icb, "retyped as a buffer")
    rewrite_entry(w.t, icb, typ, va=rva, length=rlen)
    _icb_exec(w, icb, "an ICB entry again, the buffer object live")
    s.submit(*delete_resource(1, icb), label="37: the buffer")
    _icb_exec(w, icb, "an ICB made again")
    w.out_dump("the new command ran again", 0x80)
    s.events("end")
    return s


# ---- textures and views across pause, suspend and deletes ----

TEX_USAGE = 0x13  # read, write, pixel-format view


def _tex_rig(s, n=1):
    t = Lab(s, pages=80)
    texs = [tex2d(t, contents=texels(4, 4, seed=k + 1), usage=TEX_USAGE) for k in range(n)]
    views = [view(t, ref) for ref, _ in texs]
    out, ova = t.shared(listed=False)
    t.ready()
    return t, texs, views, out, ova


@scenario
def interplay_lifetime_texture_pause():
    """A texture (type 2) and a view of it (type 8, commandType 7) across a
    pause. The GPU fills the texture blue (flags 0, so its guest page keeps
    the guest's texels) and the view reads blue. The guest rewrites the
    texture's page while paused; after unpause the texture and the view
    still read blue: pause keeps every task and resource, and a host-valid
    texture is not paged again. Listed with invalidateHost (1) the texture
    is paged from the guest's new bytes, and the view, which shares its
    storage, reads them too. The same in every setup."""
    s = _scn(interplay_lifetime_texture_pause)
    s.bring_up()
    t, ((tex, tva),), (v,), out, ova = _tex_rig(s)
    t.run(blit(tex_fill(tex, (0.0, 0.0, 1.0, 1.0))), resources=[(tex, 0), (out, 0)], label="fill blue")
    t.run(blit(tex_out(v, out, 0)), resources=[(out, 0)], label="read the view")
    s.api("pause")
    t.write(tva, bytes([0x50 + i % 16 for i in range(0x40)]))
    s.api("unpause")
    t.run(blit(tex_out(tex, out, 0x40), tex_out(v, out, 0x80)), resources=[(tex, 0), (out, 0)],
          label="after unpause: the texture and the view")
    t.run(blit(tex_out(tex, out, 0xC0), tex_out(v, out, 0x100)), resources=[(tex, IH), (out, 0)],
          label="the texture listed with flags 1")
    t.dump(ova, 0x140, "the view; texture and view after unpause; after flags 1")
    t.dump(tva, 0x40, "the texture's guest page")
    s.events("end")
    return s


@scenario
def interplay_lifetime_texture_suspend():
    """Two textures, each with a view, across a suspend and a resume on a
    fresh device. The GPU fills texture 1 blue with flags 0 and texture 2
    green with invalidateGuest (2): rvg_device_finish_suspend's flush writes
    texture 2 back to its guest page and leaves texture 1's alone. While
    suspended the guest rewrites texture 1's page. The resumed device makes
    every object again from the guest: each texture from its page, each view
    from its rebuilt base. So texture 1 and its view read the guest's new bytes
    (the blue fill is lost) and texture 2 and its view read green. The same in
    every setup."""
    from pgtest.lifecycle_util import resume_fresh, suspend
    s = _scn(interplay_lifetime_texture_suspend)
    s.bring_up()
    t, ((t1, v1a), (t2, v2a)), (w1, w2), out, ova = _tex_rig(s, 2)
    t.run(blit(tex_fill(t1, (0.0, 0.0, 1.0, 1.0)), tex_fill(t2, (0.0, 1.0, 0.0, 1.0))),
          resources=[(t1, 0), (t2, IG), (out, 0)], label="fill 1 blue (flags 0), 2 green (flags 2)")
    t.run(blit(tex_out(w1, out, 0), tex_out(w2, out, 0x40)), resources=[(out, 0)], label="read both views")
    suspend(s)
    t.dump(v1a, 0x40, "texture 1's page after finishSuspend")
    t.dump(v2a, 0x40, "texture 2's page after finishSuspend")
    t.write(v1a, bytes([0x60 + i % 16 for i in range(0x40)]))
    resume_fresh(s)
    t.run(blit(tex_out(t1, out, 0x80), tex_out(w1, out, 0xC0), tex_out(t2, out, 0x100), tex_out(w2, out, 0x140)),
          resources=[(t1, 0), (t2, 0), (out, 0)], label="resumed: both textures and both views")
    t.dump(ova, 0x180, "the views before; texture 1, view 1, texture 2, view 2 after")
    s.events("end")
    return s


@scenario
def interplay_lifetime_view_outlives_base():
    """A view holds its base's Metal texture, not its reference. The
    base is filled red and deleted (37); the next exec makes the base
    again from its guest page (the guest's texels), while the live view
    still reads red. A green fill of the new base does not reach the
    view; the view deleted and made again is a view of the new base, and
    reads green. A texture over a shared buffer (type 8, commandType 9)
    whose buffer is deleted keeps the old buffer: its fill still lands
    in the buffer's guest page in every setup."""
    s = _scn(interplay_lifetime_view_outlives_base)
    s.bring_up()
    t = Lab(s, pages=80)
    base, bva = tex2d(t, contents=texels(4, 4), usage=TEX_USAGE)
    v = view(t, base)
    sb, sbva = t.shared(contents=b"\x44" * PAGE, listed=False)
    from pgtest.resources_build import ser_on_buffer
    tb = t.add(abi.T_SERIALIZER_TEXTURE, ser_on_buffer(sb, 0, 16, desc1(usage=TEX_USAGE, options=0)))
    out, ova = t.shared(listed=False)
    t.ready()
    t.run(blit(tex_fill(base, (1.0, 0.0, 0.0, 1.0))), resources=[(base, 0), (out, 0)], label="fill the base red")
    t.run(blit(tex_out(v, out, 0x140)), resources=[(out, 0)], label="the view, made over the red base")
    s.submit(*delete_resource(1, base), label="37: the base")
    t.run(blit(tex_out(base, out, 0), tex_out(v, out, 0x40)), resources=[(base, 0), (out, 0)],
          label="the base made again, and the view")
    t.run(blit(tex_fill(base, (0.0, 1.0, 0.0, 1.0))), resources=[(base, 0), (out, 0)], label="fill the new base green")
    t.run(blit(tex_out(v, out, 0x80)), resources=[(out, 0)], label="the view")
    s.submit(*delete_resource(1, v), label="37: the view")
    t.run(blit(tex_out(v, out, 0xC0)), resources=[(out, 0)], label="the view made again")
    t.dump(ova + 0x140, 0x40, "the view over the red base")
    t.dump(ova, 0x100, "new base, view; view after the green fill; view made again")
    t.run(blit(tex_out(tb, out, 0x100)), resources=[(sb, 0), (out, 0)], label="the texture over the buffer")
    s.submit(*delete_resource(1, sb), label="37: its buffer")
    t.run(blit(tex_fill(tb, (0.0, 0.0, 1.0, 1.0))), resources=[(sb, 0), (out, 0)], label="fill the texture blue")
    t.dump(ova + 0x100, 0x40, "the texture over the buffer, before")
    t.dump(sbva, 0x40, "the buffer's guest page")
    s.events("end")
    return s


# ---- the resident set across a pause, and after a discard ----

@scenario
def interplay_lifetime_pause_resident():
    """A private buffer and a private heap (with a buffer in it) written by
    the GPU with flags 0, then the device paused; the guest rewrites their
    pages while paused. After unpause an exec with flags 0 reads them:
    in-process each is a host copy that pause keeps and does not page
    again, so the GPU's bytes read back and the guest's are unseen; in the
    service both are the guest pages, so the guest's bytes read back. An
    exec with invalidateHost (1) then pages the guest's bytes in every
    setup."""
    s = _scn(interplay_lifetime_pause_resident)
    s.bring_up()
    t = Lab(s, pages=80)
    p, pva = t.shared(contents=b"\x70" * PAGE, flags=1, listed=False)
    hva = t.reserve(2 * PAGE)
    t.write(hva, b"\x71" * 2 * PAGE)
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, 2 * PAGE, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, PAGE, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()

    def read(off, flags, label):
        t.run(blit(B.copy_buffer_to_buffer(p, out, 0, off, 0x20), B.copy_buffer_to_buffer(hb, out, 0, off + 0x20, 0x20)),
              resources=[(p, flags), (h, flags), (hb, 0), (out, 0)], label=label)

    t.run(blit(B.fill(p, 0, 0x20, 0xC1), B.fill(hb, 0, 0x20, 0xC2)), resources=[(p, 0), (h, 0), (hb, 0), (out, 0)],
          label="GPU fills, flags 0")
    s.api("pause")
    t.write(pva, b"\x81" * 0x20)
    t.write(hva + PAGE, b"\x82" * 0x20)
    s.api("unpause")
    read(0, 0, "after unpause, flags 0")
    read(0x40, IH, "flags 1")
    t.dump(ova, 0x80, "reads")
    s.events("end")
    return s


@scenario
def interplay_lifetime_discard_then_hole():
    """Resources discarded (63) and then used after the guest punched a hole
    (leaf 0) under their pages. In-process a shared buffer's discard drops
    its Metal buffer, and listed again it makes a new one over the hole:
    "Failed to create buffer", a fault; a private buffer and a legacy heap
    drop their transfer buffers and turn volatile, stay host-valid, and run
    with flags 0, while invalidateHost makes them page through a new
    transfer buffer over the hole ("Failed to create buffer" for the private
    buffer; "Failed to create transfer buffer"), a fault. The blocks setup's
    host task keeps the old pages mapped, so there nothing faults. In the
    service each is a detach resource whose discard detaches its backing,
    and attaching it again walks the hole. With the leaves restored
    everything runs again."""
    s = _scn(interplay_lifetime_discard_then_hole)
    s.bring_up()
    t = Lab(s, pages=80)
    sh, shva = t.shared(contents=b"\x31" * PAGE, listed=False)
    p, pva = t.shared(contents=b"\x32" * PAGE, flags=1, listed=False)
    hva = t.reserve(PAGE)
    t.write(hva, b"\x33" * PAGE)
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, PAGE, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    reads = {sh: 0, p: 0x20, hb: 0x40}

    def use(ref, res, label, k):
        run_either(t, blit(B.copy_buffer_to_buffer(ref, out, 0, 0x80 * k + reads[ref], 0x20)), res + [(out, 0)], label)

    use(sh, [(sh, 0)], "the shared buffer", 0)
    use(p, [(p, 0)], "the private buffer", 0)
    use(hb, [(h, 0), (hb, 0)], "the heap buffer", 0)
    s.submit(abi.OP_DISCARD_RESOURCES, refs_payload(1, [sh, p, h]), label="63: all three")
    leaves = [(va, t.table_gpa(0, 0, va // PAGE)) for va in (shva, pva, hva)]
    for va, gpa in leaves:
        s.u32(gpa, 0)
    use(sh, [(sh, 0)], "the shared buffer over a hole", 1)
    use(p, [(p, 0)], "the private buffer over a hole, flags 0", 1)
    use(p, [(p, IH)], "the private buffer over a hole, flags 1", 2)
    use(hb, [(h, 0), (hb, 0)], "the heap over a hole, flags 0", 1)
    use(hb, [(h, IH), (hb, 0)], "the heap over a hole, flags 1", 2)
    for va, gpa in leaves:
        s.u32(gpa, s.pfn(t.leaves[va // PAGE]) | abi.PTE_WRITABLE)
    use(sh, [(sh, IH)], "the shared buffer, leaves restored", 3)
    use(p, [(p, IH)], "the private buffer, leaves restored", 3)
    use(hb, [(h, IH), (hb, 0)], "the heap, leaves restored", 3)
    t.dump(ova, 0x200, "reads")
    s.events("end")
    return s


def _icb_fields(w, icb, label):
    """What a compute ICB's write-back puts in its first command's guest
    bytes, host values left out: the type @0, the pipeline reference @8,
    bind 0's reference @0x10 and offset @0x1c (its GPU address @0x14 is
    the host's; icb_cmds.COMPUTE)."""
    va = w.icbs[icb][0]
    w.t.dump(va, 4, label + ": type")
    w.t.dump(va + 8, 4, label + ": pipeline reference")
    w.t.dump(va + 0x10, 4, label + ": bind 0 reference")
    w.t.dump(va + 0x1C, 8, label + ": bind 0 offset")


@scenario(setups=("isolate",), host=True)
def interplay_lifetime_icb_writeback_after_delete():
    """What an ICB's write-back (53, isolate only: in-process it aborts,
    docs/testing.md section 7.4) says about objects deleted and made again
    since it was prepared. The service resolves a command's bind from its
    GPU address through the addresses buffers registered at creation, and a
    pipeline from its unique identifier through the pipelines registered at
    creation; deletes never unregister. A bound buffer deleted (37) is not
    found ("Resource not found for GPU address": the bind is written as
    reference ~0, offset 0); made again by an exec, the old address finds
    the old entry, whose reference names the new buffer, so the bind is
    written as before. A deleted pipeline (40, operation 1006) is still
    named by its reference. A second pipeline built from an identical
    record (over a function of its own) registers whatever unique
    identifier Metal gives it; on this host Metal gives it a new one, so
    the ICB's command still names the first reference. Only references and
    offsets are dumped; keyed by the host because Metal's pipeline identity
    is the host's."""
    from scenarios.icb import ICBTask
    from pgtest.icb_cmds import compute_cmd
    s = _scn(interplay_lifetime_icb_writeback_after_delete, setups=("isolate",), host=True)
    s.bring_up()
    w = ICBTask(s)
    o2, o2va = w.t.buffer(PAGE)
    cpso2 = w.t.compute_pipeline(icb=True)
    icb = w.icb([compute_cmd(pso=w.cpso, binds=[(o2, 0, 0x20)], grid=(4, 1, 1), tg=(4, 1, 1))])
    va, size = w.icbs[icb]
    w.start()
    _icb_exec(w, icb, "the ICB, invalidateGuest", flags=IG, res_extra=[(o2, 0)], use=[o2])

    def writeback(label):
        w.t.fill(va, size, 0xEE)
        s.submit(abi.OP_INVALIDATE_RESOURCES, invalidate_payload(1, [(icb, (0, 0, 1, 0))]), label=label + ": 52")
        s.submit(abi.OP_SYNCHRONIZE_RESOURCES, refs_payload(1, [icb]), label=label + ": 53")
        _icb_fields(w, icb, label)

    writeback("as prepared")
    s.submit(*delete_resource(1, o2), label="37: the bound buffer")
    writeback("the bound buffer deleted")
    w.t.exec_segments(blit(B.fill(o2, 0x100, 4, 0)), resources=[(o2, 0)], label="the bound buffer made again")
    writeback("the bound buffer made again")
    s.submit(*delete_object(1, 1006, w.cpso), label="40: the pipeline")
    writeback("the pipeline deleted")
    w.t.exec_segments([seg(abi.ENC_COMPUTE, C.set_pipeline(cpso2) + C.set_buffers(0, [(w.out, 0)])
                           + C.dispatch_threads((4, 1, 1), (4, 1, 1)))], resources=[(w.out, 0)],
                      label="a second pipeline from the same record")
    writeback("a second pipeline made")
    s.events("end")
    return s


def _icb_suspend(guest_invalid):
    from scenarios.icb import ICBTask
    from pgtest.icb_cmds import compute_cmd
    from pgtest.lifecycle_util import resume_fresh, suspend
    name = "interplay_lifetime_icb_suspend" + ("_evicted" if guest_invalid else "")
    fn = interplay_lifetime_icb_suspend_evicted if guest_invalid else interplay_lifetime_icb_suspend
    s = Scenario(name, setups=("isolate",) if guest_invalid else ("blocks", "map", "isolate"), doc=fn.__doc__)
    s.bring_up()
    w = ICBTask(s)
    O = w.out
    icb = w.icb([compute_cmd(pso=w.cpso, binds=[(O, 0, 0)], grid=(4, 1, 1), tg=(4, 1, 1))])
    va, _ = w.icbs[icb]
    w.start()
    _icb_exec(w, icb, "the ICB" + (", invalidateGuest" if guest_invalid else ""), flags=IG if guest_invalid else 0)
    w.out_dump("four u32s at 0", 0x80)
    w.clear_out()
    w.t.write(va, compute_cmd(pso=w.cpso, binds=[(O, 0, 0x40)], grid=(8, 1, 1), tg=(8, 1, 1)))
    suspend(s)
    w.t.dump(va, 4, "the ICB's guest command after finishSuspend: type")
    w.t.dump(va + 0x70, 0x30, "the ICB's guest command after finishSuspend: grid and threadgroup")
    resume_fresh(s)
    _icb_exec(w, icb, "resumed: the ICB")
    w.out_dump("resumed", 0x80)
    s.events("end")
    return s


@scenario
def interplay_lifetime_icb_suspend():
    """A compute ICB prepared and run, then its guest command rewritten
    (another offset and size), which the live ICB ignores. Across a suspend
    and a resume on a fresh device the ICB is made again from its guest
    memory (every object is), so the resumed device runs the rewritten
    command. Listed with flags 0 the ICB is guest-valid, so
    rvg_device_finish_suspend's flush leaves its guest memory alone."""
    return _icb_suspend(False)


@scenario(setups=("isolate",))
def interplay_lifetime_icb_suspend_evicted():
    """interplay_lifetime_icb_suspend with the ICB listed with
    invalidateGuest (2): rvg_device_finish_suspend's flush evicts it through
    the CPU, writing the prepared command back over the guest's rewrite, so the
    resumed device runs the old command. Isolate only: in-process that eviction
    aborts the host (icb_evict_in_process)."""
    return _icb_suspend(True)


@scenario
def interplay_lifetime_heap_suspend():
    """Two private heaps across a suspend and a resume on a fresh device.
    Heap 1's buffer is filled with flags 0, heap 2's with invalidateGuest
    (2), and while suspended the guest rewrites another page of heap 1.
    In-process rvg_device_finish_suspend's flush writes heap 2 back and not
    heap 1, and the resumed heaps are paged from the guest: heap 1's fill is
    lost, its rewritten page is read, heap 2's fill survives. In the service
    both heaps are the guest pages and every fill survives."""
    from pgtest.lifecycle_util import resume_fresh, suspend
    s = _scn(interplay_lifetime_heap_suspend)
    s.bring_up()
    t = Lab(s, pages=80)
    h1va = t.reserve(H4)
    t.write(h1va, pattern(H4, 5, 3))
    h1 = t.add(abi.T_HEAP, heap_rec(h1va // PAGE, H4, 1))
    b1 = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h1, 0, PAGE))
    b2 = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h1, 2 * PAGE, PAGE))
    h2va = t.reserve(2 * PAGE)
    t.write(h2va, pattern(2 * PAGE, 3, 7))
    h2 = t.add(abi.T_HEAP, heap_rec(h2va // PAGE, 2 * PAGE, 1))
    b3 = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h2, PAGE, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    res = [(h1, 0), (h2, 0), (b1, 0), (b2, 0), (b3, 0), (out, 0)]
    t.run(blit(B.fill(b1, 0, 0x20, 0xA1), B.fill(b3, 0, 0x20, 0xA3)),
          resources=[(h1, 0), (h2, IG), (b1, 0), (b3, 0), (out, 0)], label="fill heap 1 (flags 0) and heap 2 (flags 2)")
    suspend(s)
    t.dump(h1va, 0x20, "heap 1's first page after finishSuspend")
    t.dump(h2va + PAGE, 0x20, "heap 2's second page after finishSuspend")
    t.write(h1va + 2 * PAGE, b"\x5C" * 0x20)
    resume_fresh(s)
    t.run(blit(B.copy_buffer_to_buffer(b1, out, 0, 0, 0x20), B.copy_buffer_to_buffer(b2, out, 0, 0x20, 0x20),
               B.copy_buffer_to_buffer(b3, out, 0, 0x40, 0x20)), resources=res, label="resumed: the three buffers read")
    t.dump(ova, 0x60, "heap 1 at 0 and 0x8000, heap 2 at 0x4000")
    s.events("end")
    return s


@scenario
def interplay_lifetime_task_delete_loses_dirty():
    """DeleteTask (32) tears the task down without a flush (unlike
    rvg_device_finish_suspend's): a private buffer, a texture and a private
    heap the GPU wrote with invalidateGuest (2) are gone with their writes.
    In-process their guest pages never saw the writes, and the task defined
    again pages them in from the guest as they were. In the service the buffer
    and the heap were the guest pages, so their fills are there; the texture is
    a host copy there too, and its fill is lost."""
    s = _scn(interplay_lifetime_task_delete_loses_dirty)
    s.bring_up()
    t = Lab(s, pages=80)
    p, pva = t.shared(contents=b"\x10" * PAGE, flags=1, listed=False)
    tex, tva = tex2d(t, contents=texels(4, 4), usage=TEX_USAGE)
    hva = t.reserve(PAGE)
    t.write(hva, b"\x30" * PAGE)
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, PAGE, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    t.run(blit(B.fill(p, 0, 0x20, 0xA1), tex_fill(tex, (1.0, 1.0, 0.0, 1.0)), B.fill(hb, 0, 0x20, 0xA3)),
          resources=[(p, IG), (tex, IG), (h, IG), (hb, 0), (out, 0)], label="GPU writes, invalidateGuest")
    s.submit(abi.OP_DELETE_TASK, struct.pack("<I", 1), label="32")
    t.dump(pva, 0x20, "the private buffer's page")
    t.dump(tva, 0x40, "the texture's page")
    t.dump(hva, 0x20, "the heap's page")
    t.ready()
    t.run(blit(B.copy_buffer_to_buffer(p, out, 0, 0, 0x20), tex_out(tex, out, 0x20),
               B.copy_buffer_to_buffer(hb, out, 0, 0x60, 0x20)), resources=[(p, 0), (tex, 0), (h, 0), (hb, 0), (out, 0)],
          label="the task defined again: all three read")
    t.dump(ova, 0x80, "reads")
    s.events("end")
    return s


@scenario
def interplay_lifetime_suspend_root_word_rewritten():
    """rvg_device_did_resume rebuilds each task from its root word and the task
    root it names. While suspended the guest points task 1's root word at task
    2's root and clears task 2's word: the resumed task 1 is task 2's page
    table and object list under ID 1, and task 2 is gone. So task 1's exec, the
    same payload as before, runs task 2's command at the same VA over task 2's
    objects: task 2's result buffer gets task 2's new source bytes, task 1's
    own result buffer is untouched, and an exec of task 2 faults ("Invalid
    task")."""
    from pgtest.lifecycle_util import resume_fresh, suspend
    s = _scn(interplay_lifetime_suspend_root_word_rewritten)
    s.bring_up()
    ts = []
    for tid, byte in ((1, 0x11), (2, 0x22)):
        t = Lab(s, tid=tid)
        src, sva = t.shared(contents=bytes([byte]) * PAGE, listed=False)
        out, ova = t.shared(listed=False)
        t.ready()
        cbs = t.place(blit(B.copy_buffer_to_buffer(src, out, 0, 0, 0x40)))
        ts.append((t, src, sva, out, ova, cbs))
    from pgtest.diff_util import exec_payload
    for t, src, sva, out, ova, cbs in ts:
        s.submit(abi.OP_EXEC_INDIRECT3, exec_payload(t.tid, cbs, [(src, 0), (out, 0)]), label="task %d's copy" % t.tid)
    suspend(s)
    (t1, src1, _, out1, o1va, cbs1), (t2, src2, s2va, out2, o2va, cbs2) = ts
    s.u32(s.pa(s.root_page, 4 * 1), s.pfn(t2.root))
    s.u32(s.pa(s.root_page, 4 * 2), 0)
    t2.write(s2va, b"\x33" * 0x40)
    t2.write(o2va, bytes(0x40))
    t1.write(o1va, bytes(0x40))
    resume_fresh(s)
    s.submit(abi.OP_EXEC_INDIRECT3, exec_payload(1, cbs1, [(src1, 0), (out1, 0)]), label="resumed: task 1's copy")
    t1.dump(o1va, 0x40, "task 1's result buffer")
    t2.dump(o2va, 0x40, "task 2's result buffer")
    either(s, abi.OP_EXEC_INDIRECT3, exec_payload(2, cbs2, [(src2, 0), (out2, 0)]), "resumed: task 2's copy")
    s.dump(s.pa(s.root_page, 4), 8, "root words 1 and 2")
    s.events("end")
    return s


@scenario
def interplay_lifetime_discard_unlisted():
    """What a discarded resource does in an exec that does not list it, so
    that nothing pages it back. In-process a shared buffer's discard (63)
    and ReplacePhysical (60, which discards whatever the resource's kind)
    drop its Metal buffer, and the unlisted use throws "Getting buffer from
    invalidated resource", a fault; listed, it is made again and the exec
    runs. A heap buffer's discard does nothing; a heap's 60 makes it
    volatile and it stays host-valid. In the service each is a detach
    resource whose discard detaches its backing: used unlisted, the GPU
    finds no memory under it and the exec faults; listed, the backing is
    attached again."""
    s = _scn(interplay_lifetime_discard_unlisted)
    s.bring_up()
    t = Lab(s, pages=80)
    sh, shva = t.shared(contents=b"\x41" * PAGE, listed=False)
    hva = t.reserve(PAGE)
    t.write(hva, b"\x42" * PAGE)
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, PAGE, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    k = [0]

    def read(ref, res, label):
        run_either(t, blit(B.copy_buffer_to_buffer(ref, out, 0, 0x20 * k[0], 0x20)), res + [(out, 0)], label)
        k[0] += 1

    read(sh, [(sh, 0)], "the shared buffer, listed")
    read(hb, [(h, 0), (hb, 0)], "the heap buffer, listed")
    s.submit(abi.OP_DISCARD_RESOURCES, refs_payload(1, [sh, hb]), label="63: the buffer and the heap buffer")
    read(sh, [], "the shared buffer after 63, unlisted")
    read(hb, [], "the heap buffer after 63, unlisted")
    read(sh, [(sh, 0)], "the shared buffer, listed again")
    s.submit(abi.OP_REPLACE_PHYSICAL, struct.pack("<II", 1, sh), label="60: the shared buffer")
    read(sh, [], "the shared buffer after 60, unlisted")
    read(sh, [(sh, 0)], "the shared buffer, listed again")
    s.submit(abi.OP_REPLACE_PHYSICAL, struct.pack("<II", 1, h), label="60: the heap")
    read(hb, [], "the heap buffer after its heap's 60, unlisted")
    read(hb, [(h, 0), (hb, 0)], "the heap buffer, its heap listed")
    t.dump(ova, 0x20 * k[0], "reads")
    s.events("end")
    return s


@scenario
def interplay_lifetime_texture_paging_flags():
    """interplay_lifetime_paging_flags for a normal texture (type 2): seven
    execs, the guest rewriting the texture's first row before each, each
    exec copying the texture out and filling one texel with its own colour,
    the texture listed with flags 0, 1, 2, 0, 4, 7, 0. Textures are host
    copies in every setup, so the three files agree."""
    from pgtest.resources_build import b_fill_tex_color
    s = _scn(interplay_lifetime_texture_paging_flags)
    s.bring_up()
    t = Lab(s, pages=80)
    tex, tva = tex2d(t, contents=texels(4, 4), usage=TEX_USAGE)
    out, ova = t.shared(listed=False)
    t.ready()
    from pgtest.resources_build import RGBA8Unorm as FMT
    for k, f in enumerate(P_FLAGS):
        if k:
            t.write(tva, bytes([0x10 * k + i for i in range(16)]))
        colour = (k / 8.0, 1.0 - k / 8.0, 0.5, 1.0)
        t.run(blit(tex_out(tex, out, 0x40 * k),
                   b_fill_tex_color(tex, (1, 1, 1), colour, FMT, origin=(k % 4, 1 + k // 4, 0))),
              resources=[(tex, f), (out, 0)], label="exec %d, flags %d" % (k, f))
        t.dump(tva, 0x40, "exec %d: the texture's page" % k)
    t.dump(ova, 0x40 * len(P_FLAGS), "each exec's read")
    s.submit(abi.OP_SYNCHRONIZE_RESOURCES, refs_payload(1, [tex]), label="53")
    t.dump(tva, 0x40, "after 53")
    s.events("end")
    return s


def _held_retyped(what):
    s = Scenario("interplay_lifetime_held_retyped_" + what, doc=interplay_lifetime_held_retyped.__doc__)
    if what == "icb":
        from scenarios.icb import ICBTask, exec_ranged, use_resources
        from pgtest.icb_cmds import compute_cmd
        g = Life(s, task=False)
        w = ICBTask(s)
        O = w.out
        icb = w.icb([compute_cmd(pso=w.cpso, binds=[(O, 0, 0)], grid=(4, 1, 1), tg=(4, 1, 1))])
        va, _ = w.icbs[icb]
        w.start()
        _icb_exec(w, icb, "the ICB")
        w.out_dump("four u32s at 0", 0x80)
        w.clear_out()
        w.t.reset_cmdbufs()
        ranges = [w.t.cmdbuf(seg(abi.ENC_COMPUTE, use_resources(O) + exec_ranged(icb, 0, 1)))]
        w.t.tid = 1
        st = g.hold(None, [(O, 0), (icb, 0)], "the ICB again", t=w.t, ranges=ranges)
        g.work(*delete_resource(1, icb), label="37 on the worker: the ICB")
        w.t.write(va, compute_cmd(pso=w.cpso, binds=[(O, 0, 0x40)], grid=(8, 1, 1), tg=(8, 1, 1)))
        g.release(st, "released")
        w.out_dump("released", 0x80)
    else:
        g = Life(s)
        t = g.t
        x, xva = t.shared(contents=b"\xA0" * PAGE, listed=False)
        hva = t.reserve(PAGE)
        t.write(hva, b"\xD0" * PAGE)
        out, ova = t.shared(listed=False)
        t.ready()
        t.run(blit(B.copy_buffer_to_buffer(x, out, 0, 0, 0x20)), resources=[(x, 0), (out, 0)], label="the buffer")
        st = g.hold(blit(B.copy_buffer_to_buffer(x, out, 0, 0x20, 0x20)), [(x, 0), (out, 0)], "the buffer again")
        g.work(*delete_resource(1, x), label="37 on the worker: the buffer")
        rewrite_entry(t, x, abi.T_HEAP, heap_rec(hva // PAGE, PAGE, 1))
        g.release(st, "released")
        t.dump(ova, 0x40, "reads")
    s.events("end")
    return s


@scenario
def interplay_lifetime_held_retyped():
    """An exec held on the gate while the worker deletes (37) what it names
    and the guest rewrites the entry, so the release makes the object again
    from the new entry: an ICB whose guest command was rewritten runs the
    new command; a buffer whose entry now says private heap (type 13) is,
    in-process, a legacy heap with no Resource::get_buffer (a fault), and in
    the service a detach heap whose cover buffer copies the heap's
    bytes."""
    return [_held_retyped("icb"), _held_retyped("buffer_to_heap")]


# ---- records rewritten across a suspend; one reference, several kinds ----

@scenario
def interplay_lifetime_suspend_held_record_rewritten():
    """An exec that copies buffer X out is held on the gate when the host
    suspends; rvg_device_finish_suspend abandons its wait and tears every task
    down. While suspended the guest points X's entry at another page and moves
    a heap buffer's record to another offset. On the fresh device the exec is
    parsed again and waits again (as in interplay_life_suspend_blocked);
    released, it makes X and the heap buffer from the records as they are now:
    it reads the other page, and the heap buffer at its new offset. Nothing of
    the old records survives the resume."""
    from pgtest.lifecycle_util import resume_fresh, suspend
    s = _scn(interplay_lifetime_suspend_held_record_rewritten)
    g = Life(s)
    t = g.t
    x, xva = t.shared(contents=b"\xA0" * PAGE, listed=False)
    yva = t.reserve(PAGE)
    t.write(yva, b"\xB0" * PAGE)
    hva = t.reserve(2 * PAGE)
    t.write(hva, b"\xC0" * PAGE + b"\xC1" * PAGE)
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, 2 * PAGE, 1))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    work = blit(B.copy_buffer_to_buffer(x, out, 0, 0, 0x20), B.copy_buffer_to_buffer(hb, out, 0, 0x20, 0x20))
    res = [(x, 0), (h, 0), (hb, 0), (out, 0)]
    t.run(work, resources=res, label="before")
    t.dump(ova, 0x40, "before")
    t.write(ova, bytes(0x40))
    st = g.hold(work, res, "the copy again")
    suspend(s)
    rewrite_entry(t, x, abi.T_BUFFER, buffer_rec(PAGE, yva // PAGE))
    rewrite_entry(t, hb, abi.T_HEAP_BUFFER, heap_buffer_rec(h, PAGE, PAGE))
    resume_fresh(s)
    g.f.wait_stamp(st, timeout_ms=300, label="held again after the resume")
    g.release(st, "released")
    t.dump(ova, 0x40, "after the resume")
    s.events("end")
    return s


@scenario
def interplay_lifetime_one_reference_two_kinds():
    """The object manager keeps one map per kind, and a reference names an
    object in each. A delete of the wrong kind forgets nothing: 40 with
    operation 1006 (a compute pipeline) on a buffer's reference, and 37 on
    a pipeline's, leave both live. And one reference can hold two live
    objects at once: a buffer, cached, whose entry the guest then rewrites
    into a compute pipeline's record (type 7) is made a pipeline by the
    next dispatch, while the next blit still finds the buffer; a 37 then
    drops the buffer and the next blit tries to make one from the
    pipeline's record, which fails ("Failed to create Buffer"), while the
    pipeline, in its own map, still dispatches."""
    from pgtest.compute_objects import compute_blob, compute_pipeline_record
    from pgtest.diff_util import K_DOUBLE
    s = _scn(interplay_lifetime_one_reference_two_kinds)
    s.bring_up()
    t = Lab(s, pages=80)
    fn = t.function(K_DOUBLE)
    pso = t.add(abi.T_SERIALIZED, compute_pipeline_record(compute_blob(fn)))
    x, xva = t.shared(contents=b"\x5A" * PAGE, listed=False)
    out, ova = t.shared(listed=False)
    t.ready()

    def dispatch(ref, off, label):
        run_either(t, [seg(abi.ENC_COMPUTE, C.set_pipeline(ref) + C.set_buffers(0, [(out, off)])
                           + C.dispatch_threads((8, 1, 1), (8, 1, 1)))], [(out, 0)], label)

    def copy(off, label):
        run_either(t, blit(B.copy_buffer_to_buffer(x, out, 0, off, 0x20)), [(x, 0), (out, 0)], label)

    copy(0, "the buffer")
    dispatch(pso, 0x20, "the pipeline")
    s.submit(*delete_object(1, 1006, x), label="40, operation 1006, on the buffer's reference")
    s.submit(*delete_resource(1, pso), label="37 on the pipeline's reference")
    copy(0x40, "the buffer after both")
    dispatch(pso, 0x60, "the pipeline after both")
    rewrite_entry(t, x, abi.T_SERIALIZED, compute_pipeline_record(compute_blob(fn)))
    dispatch(x, 0x80, "the buffer's reference as a pipeline")
    copy(0xA0, "the buffer's reference as a buffer")
    s.submit(*delete_resource(1, x), label="37: the buffer")
    copy(0xC0, "the reference as a buffer again")
    dispatch(x, 0xE0, "the reference as a pipeline again")
    t.dump(ova, 0x100, "results")
    s.events("end")
    return s


@scenario
def interplay_lifetime_function_swapped():
    """A compute pipeline made again from the same record over a function
    made again from a new one. The guest points the function's entry at
    another metallib (add_one where double_gid was): with the function and
    the pipeline live nothing changes; after the function's delete (40,
    operation 1005) alone the live pipeline still runs double_gid; after
    the pipeline's delete (1006) too, the next dispatch makes the pipeline
    again, which makes the function again from its new entry, and add_one
    runs."""
    from pgtest.compute_objects import compute_blob, compute_pipeline_record, function_record
    from pgtest.diff_util import K_ADD, K_DOUBLE
    import os
    from pgtest.scenario import FIXTURES
    s = _scn(interplay_lifetime_function_swapped)
    s.bring_up()
    t = Lab(s, pages=80)
    fn = t.function(K_DOUBLE)
    pso = t.add(abi.T_SERIALIZED, compute_pipeline_record(compute_blob(fn)))
    add_len = len(open(os.path.join(FIXTURES, K_ADD), "rb").read())
    add_va = t.reserve(add_len)
    s.mem_file(t.gpa(add_va), K_ADD)
    src, _ = t.shared(contents=b"".join(struct.pack("<I", 100 + i) for i in range(16)), listed=False)
    out, ova = t.shared(listed=False)
    t.ready()
    k = [0]

    def dispatch(label):
        t.run([seg(abi.ENC_COMPUTE, C.set_pipeline(pso) + C.set_buffers(0, [(out, 0x40 * k[0]), (src, 0)])
                   + C.dispatch_threads((16, 1, 1), (16, 1, 1)))], resources=[(out, 0), (src, 0)], label=label)
        k[0] += 1

    dispatch("double_gid")
    rewrite_entry(t, fn, abi.T_FUNCTION, function_record(add_va, add_len))
    dispatch("the function's entry rewritten, both live")
    s.submit(*delete_object(1, 1005, fn), label="40, operation 1005: the function")
    dispatch("the function deleted, the pipeline live")
    s.submit(*delete_object(1, 1006, pso), label="40, operation 1006: the pipeline")
    dispatch("the pipeline deleted too")
    t.dump(ova, 0x40 * k[0], "results")
    s.events("end")
    return s


@scenario
def interplay_lifetime_icb_rebound():
    """A compute ICB bound to buffer B by reference, prepared and run; B is
    deleted (37). The ICB listed with invalidateHost (1) is prepared again
    from its guest command, whose bind resolves reference B through
    getObject, which makes B again from its record: the dispatch lands in
    B's guest pages in every setup. The ICB is listed before B, so its
    prepare is what makes B."""
    from scenarios.icb import ICBTask
    from pgtest.icb_cmds import compute_cmd
    s = _scn(interplay_lifetime_icb_rebound)
    s.bring_up()
    w = ICBTask(s)
    b, bva = w.t.buffer(PAGE)
    icb = w.icb([compute_cmd(pso=w.cpso, binds=[(b, 0, 0x20)], grid=(4, 1, 1), tg=(4, 1, 1))])
    w.start()
    _icb_exec(w, icb, "the ICB, bound to B", res_extra=[(b, 0)], use=[b])
    w.t.dump(bva, 0x40, "B")
    w.t.write(bva, bytes(0x40))
    s.submit(*delete_resource(1, b), label="37: B")
    _icb_exec(w, icb, "the ICB prepared again, B made again", flags=IH, res_extra=[(b, 0)], use=[b])
    w.t.dump(bva, 0x40, "B after")
    s.events("end")
    return s


@scenario
def interplay_lifetime_held_heap_texture():
    """interplay_lifetime_held_heap_deleted for a texture placed in the heap
    (type 8, commandType 21): the texture keeps the old heap's Metal heap,
    so after the heap's delete (37, on the worker, while an exec reading the
    texture is held) the released exec lists a new heap paged from the guest
    and reads the old one's bytes through the texture: in-process the GPU
    fill of the texture (uniform, so its layout does not show) survives only
    in the old heap; in the service both heaps are the guest pages."""
    s = _scn(interplay_lifetime_held_heap_texture)
    g = Life(s)
    t = g.t
    hva = t.reserve(2 * PAGE)
    t.write(hva, b"\x6B" * 2 * PAGE)
    h = t.add(abi.T_HEAP, heap_rec(hva // PAGE, 2 * PAGE, 1))
    tex = t.add(abi.T_SERIALIZER_TEXTURE, ser_in_heap(h, 0, desc1(usage=3)))
    hb = t.add(abi.T_HEAP_BUFFER, heap_buffer_rec(h, 0, PAGE))
    out, ova = t.shared(listed=False)
    t.ready()
    t.run(blit(tex_fill(tex, (0.0, 1.0, 1.0, 1.0))), resources=[(h, 0), (out, 0)], label="fill the texture")
    st = g.hold(blit(tex_out(tex, out, 0), B.copy_buffer_to_buffer(hb, out, 0, 0x40, 0x40)),
                [(h, 0), (hb, 0), (out, 0)], "the texture and a heap buffer read")
    g.work(*delete_resource(1, h), label="37 on the worker: the heap")
    g.release(st, "released")
    t.dump(ova, 0x80, "the texture, then the heap buffer made in the new heap")
    t.dump(hva, 0x40, "the heap's guest page")
    s.events("end")
    return s


@scenario(setups=("isolate",), host=True)
def interplay_lifetime_placement_after_delete():
    """With a placement list (opcode 68; the service with argument buffers
    only, docs/testing.md section 6.11) a buffer made with a zero entry
    writes its GPU address there. DeleteResource (37) zeroes the entry, and
    the buffer made again by the next exec writes an address again. The
    addresses are the host's: a relation kernel (fixtures/shaders/rel.metal)
    compares the entry before the delete with zero and the entry after the
    re-creation with zero, so only "nonzero" is locked; the entry between is
    dumped (all zero). A buffer over the placement list's own page reads the
    entries on the GPU."""
    from pgtest.exec_objects import ObjTask
    from scenarios.textures_placement import install_with_placements
    s = _scn(interplay_lifetime_placement_after_delete, setups=("isolate",), host=True)
    s.bring_up()
    t = ObjTask(s, pages=96)
    x, xva = t.buffer(PAGE, contents=b"\x19" * PAGE)
    pairs, pva = t.buffer(PAGE)
    rel_out, rva = t.buffer(PAGE)
    t.relation_kernel()
    pl_va = t.reserve(PAGE)
    view_ref = t.ol.add(abi.T_BUFFER, buffer_rec(PAGE, pl_va // PAGE))
    t.define()
    n = len(t.ol.entries)
    t._blob = pl_va  # the placement list goes on the page the view names
    got = install_with_placements(t, {})
    assert got == pl_va
    res = [(x, 0), (pairs, 0), (view_ref, 0)]

    def run(records, label):
        t.exec_segments(blit(*records), resources=res, label=label)

    run([B.fill(x, 0, 0x10, 0x55)], "the buffer made")
    run([B.copy_buffer_to_buffer(view_ref, pairs, 16 * x, 0, 8)], "its entry's address copied: pair 0 = (address, 0)")
    s.submit(*delete_resource(1, x), label="37")
    t.dump(pl_va + 16 * x, 16, "the entry after the delete")
    run([B.fill(x, 0x10, 0x10, 0x66)], "the buffer made again")
    run([B.copy_buffer_to_buffer(view_ref, pairs, 16 * x, 0x18, 8)], "its entry's address copied: pair 1 = (0, address)")
    t.relate(pairs, rel_out, 2)
    t.dump(rva, 8, "relations: bit 1 equal, 2 first nonzero, 4 second nonzero")
    t.dump(xva, 0x20, "the buffer's pages")
    s.events("end")
    return s


@scenario
def interplay_lifetime_private_across_fifos():
    """A private buffer filled on the worker FIFO (child 2) by an exec that
    lists it with invalidateGuest (2), then read on the root FIFO by execs
    gated on the worker's stamp. In-process the buffer is one host copy
    shared by every FIFO: read with flags 0 it shows the fill; read with
    invalidateHost (1), it is paged from its guest pages, which never saw
    the fill (invalidateHost makes the guest's copy the valid one, and
    nothing synchronized it), so the fill is gone for good, and a later
    read with flags 0 shows the guest's bytes. In the service the buffer is
    the guest pages, and every read shows the fill."""
    from pgtest.diff_util import exec_payload
    s = _scn(interplay_lifetime_private_across_fifos)
    g = Life(s)
    t = g.t
    p, pva = t.shared(contents=b"\x0F" * PAGE, flags=1, listed=False)
    out, ova = t.shared(listed=False)
    t.ready()
    w = g.worker
    fill = w.submit(abi.OP_EXEC_INDIRECT3, exec_payload(1, t.place(blit(B.fill(p, 0, 0x20, 0xF1))), [(p, IG), (out, 0)]),
                    label="the worker fills it, flags 2")
    for k, f in enumerate((0, IH, 0)):
        st = g.f.cmd(abi.OP_EXEC_INDIRECT3,
                     exec_payload(1, t.place(blit(B.copy_buffer_to_buffer(p, out, 0, 0x20 * k, 0x20))), [(p, f), (out, 0)]),
                     barriers=[(w.stamp_index, fill)])
        g.f.doorbell()
        g.f.wait_stamp(st, label="the root reads it, flags %d" % f)
    t.dump(ova, 0x60, "three reads")
    t.dump(pva, 0x20, "its guest page")
    s.events("end")
    return s


@scenario(exclusive=True)
def interplay_lifetime_icb_pipeline_deleted():
    """A compute ICB whose command's pipeline is deleted (40, operation 1006)
    after the ICB was prepared: the ICB, run again as prepared (flags 0), still
    dispatches with the deleted pipeline, and nothing faults (checked in the
    system log while blessing). Listed with invalidateHost it is prepared
    again, which makes the pipeline again from its record, and runs the same.
    Exclusive, as interplay_lifetime_icb_bound_buffer_deleted."""
    from scenarios.icb import ICBTask
    from pgtest.icb_cmds import compute_cmd
    s = Scenario("interplay_lifetime_icb_pipeline_deleted", exclusive=True,
                 doc=interplay_lifetime_icb_pipeline_deleted.__doc__)
    s.bring_up()
    w = ICBTask(s)
    O = w.out
    icb = w.icb([compute_cmd(pso=w.cpso, binds=[(O, 0, 0)], grid=(8, 1, 1), tg=(8, 1, 1))])
    w.start()
    _icb_exec(w, icb, "the ICB")
    w.out_dump("before", 0x40)
    w.clear_out()
    s.submit(*delete_object(1, 1006, w.cpso), label="40, operation 1006: the ICB's pipeline")
    _icb_exec(w, icb, "the ICB again, its pipeline deleted")
    w.out_dump("after", 0x40)
    w.clear_out()
    _icb_exec(w, icb, "the ICB invalidated", flags=IH)
    w.out_dump("the ICB prepared again", 0x40)
    s.events("end")
    return s
