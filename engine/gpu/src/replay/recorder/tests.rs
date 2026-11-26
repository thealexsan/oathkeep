use super::*;

use super::*;

#[test]
fn when_to_record() {
    assert_eq!(parse_at("120"), At::Seconds(120.0));
    assert_eq!(parse_at(" 1.5 "), At::Seconds(1.5));
    assert_eq!(parse_at("f900"), At::Frame(900));
    assert_eq!(parse_at(""), At::Trigger);
    assert_eq!(parse_at("later"), At::Trigger);
}

#[test]
fn descriptor_heaps_are_found_by_address() {
    let heap = |id, cpu, gpu, count| {
        let d = D3D12_DESCRIPTOR_HEAP_DESC {
            NumDescriptors: count,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            ..Default::default()
        };
        Arc::new(Heap::new(id, &d, cpu, gpu, 32))
    };
    let mut all = Heaps::default();
    all.add(heap(1, 0x1000, 0x10_0000, 4));
    all.add(heap(2, 0x2000, 0x20_0000, 8));
    let at = |all: &Heaps, p| all.cpu(p).map(|(h, i)| (h.id, i));
    assert_eq!(at(&all, 0x1000), Some((1, 0)));
    assert_eq!(at(&all, 0x1060), Some((1, 3)));
    assert_eq!(at(&all, 0x1080), None, "past its end");
    assert_eq!(at(&all, 0xfff), None);
    assert_eq!(all.gpu(0x20_0040).map(|(h, i)| (h.id, i)), Some((2, 2)));
    // A heap made where a dead one was takes its place.
    all.add(heap(3, 0x1020, 0x30_0000, 4));
    assert_eq!(at(&all, 0x1000), None);
    assert_eq!(at(&all, 0x1020), Some((3, 0)));
    all.remove(2);
    assert_eq!(at(&all, 0x2000), None);
    let h = heap(4, 0, 0, 2);
    let s = Slot {
        kind: slot::SRV,
        res: 5,
        ..Slot::EMPTY
    };
    h.set(1, &s);
    h.set(2, &s);
    assert_eq!(h.all(), [Slot::EMPTY, s]);
    assert_eq!(h.get(9), Slot::EMPTY);
}
