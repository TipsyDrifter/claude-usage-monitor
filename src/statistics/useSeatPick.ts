import { useEffect, useState } from "react";
import { useStore } from "@/store/usageStore";
import { cmd } from "@/lib/tauri";

// =============================================================================
// v1.8.13（D91 T-15）：趨勢／歷史頁要看哪一組帳號×組織。
//
// 以前兩頁的 seatPick 初始是 null，掛載第一輪就用 null 打一次後端；後端把 null 對到
// 「樣本最多的帳號」（不一定是目前帳號）冷算整頁、佔著帳本鎖，那把快取 key 還永久留著，
// 之後每輪採集都多重算一份別人的頁面。現在直接從 store 推：頂欄選的 ?? 目前登入的 ??
// 清單第一個（有帳號但沒有「目前登入」時，list_seats 按樣本數排，明講 id 不丟 null）。
//
// store 的座位清單是非同步載的，「還沒載到」和「清單真的是空的」長得一樣。
// 所以 store 給不出答案時自己問一次 list_seats（每次採集成功再問一次，
// 第一次採集完成、帳號進帳本的那一刻頁面就接得上）：
//   ready=false          還在問——頁面顯示骨架、不發請求
//   seat=id, ready       list_seats 有帳號——用它
//   seat=null, ready     list_seats 是空的或失敗——用 null 問，「帳本是不是空的」交給後端的 empty 判斷
//
// v1.8.13（D91 T-03 審查）：以前 list_seats 空就判「帳本沒帳號」直接顯示空狀態、不發請求。
// 但 list_seats 只列有帳號 uuid 的座位，趨勢／歷史頁的後端看的是整張 seats 表——
// 只用 Desktop、bridge-state 查不到「組織→帳號」對照時，樣本記在沒有帳號 uuid 的暫存座位上，
// list_seats 永遠是空的，兩頁就永遠卡在「還沒有資料」。null 在這種帳本上只會對到那個暫存座位，
// 不會像 T-15 那樣冷算到別人的頁面。
// =============================================================================

export interface SeatPick {
  seat: string | null;
  ready: boolean;
}

export function useSeatPick(): SeatPick {
  const viewSeatId = useStore((s) => s.viewSeatId);
  const currentSeatId = useStore((s) => s.currentSeatId);
  const firstSeatId = useStore((s) => s.seats[0]?.id ?? null);
  const lastSuccessAt = useStore((s) => s.usage.lastSuccessAt);
  const fromStore = viewSeatId ?? currentSeatId ?? firstSeatId;
  const [probe, setProbe] = useState<{ seat: string | null } | null>(null);

  useEffect(() => {
    if (fromStore) return;
    let alive = true;
    cmd
      .listSeats()
      .then((r) => {
        if (!alive) return;
        setProbe({ seat: r.currentSeatId || r.seats[0]?.id || null });
      })
      .catch(() => {
        if (alive) setProbe({ seat: null });
      });
    return () => {
      alive = false;
    };
  }, [fromStore, lastSuccessAt]);

  if (fromStore) return { seat: fromStore, ready: true };
  if (!probe) return { seat: null, ready: false };
  return { seat: probe.seat, ready: true };
}
