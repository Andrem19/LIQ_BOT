use crate::types::{RangeAlloc, PriceBound, BoundType};

pub fn calc_bound_prices_struct(base_price: f64, pct_list: &[f64]) -> Vec<PriceBound> {
    assert!(pct_list.len() == 4, "Нужно ровно 4 процента: [верх_внутр, низ_внутр, верх_экстр, низ_экстр]");

    // Верхняя внутренняя граница (немного выше рынка)
    let upper_inner = base_price * (1.0 + pct_list[0]);
    // Нижняя внутренняя граница (немного ниже рынка)
    let lower_inner = base_price * (1.0 - pct_list[1]);
    // Верхняя экстремальная (ещё выше)
    let upper_outer = upper_inner * (1.0 + pct_list[2]);
    // Нижняя экстремальная (ещё ниже)
    let lower_outer = lower_inner * (1.0 - pct_list[3]);

    vec![
        PriceBound { bound_type: BoundType::UpperOuter, value: upper_outer },
        PriceBound { bound_type: BoundType::UpperInner, value: upper_inner },
        PriceBound { bound_type: BoundType::LowerInner, value: lower_inner },
        PriceBound { bound_type: BoundType::LowerOuter, value: lower_outer },
    ]
}

pub fn calc_range_allocation_struct(
    price: f64,
    bounds: &[PriceBound],
    weights: &[f64; 3],
    total_usdc: f64,
) -> Vec<RangeAlloc> {
    // Достаем нужные границы по типу:
    let get = |t: BoundType| bounds.iter().find(|b| b.bound_type == t).unwrap().value;

    let upper_outer = get(BoundType::UpperOuter);
    let upper_inner = get(BoundType::UpperInner);
    let lower_inner = get(BoundType::LowerInner);
    let lower_outer = get(BoundType::LowerOuter);

    // Верхний диапазон — между upper_outer и upper_inner
    let upper_weight = total_usdc * weights[0] / 100.0;
    let upper_sol = upper_weight / price;
    let upper = RangeAlloc {
        range_idx: 0,
        range_type: "upper",
        usdc_amount: 0.0,
        sol_amount: upper_sol,
        usdc_equivalent: upper_sol * price,
        upper_price: upper_outer,
        lower_price: upper_inner,
    };

    // Центральный диапазон — между upper_inner и lower_inner
    let center_weight = total_usdc * weights[1] / 100.0;
    let center_sol = (center_weight / 2.0) / price;
    let center_usdc = center_weight / 2.0;
    let center = RangeAlloc {
        range_idx: 1,
        range_type: "center",
        usdc_amount: center_usdc,
        sol_amount: center_sol,
        usdc_equivalent: center_usdc + center_sol * price,
        upper_price: upper_inner,
        lower_price: lower_inner,
    };

    // Нижний диапазон — между lower_inner и lower_outer
    let lower_weight = total_usdc * weights[2] / 100.0;
    let lower = RangeAlloc {
        range_idx: 2,
        range_type: "lower",
        usdc_amount: lower_weight,
        sol_amount: 0.0,
        usdc_equivalent: lower_weight,
        upper_price: lower_inner,
        lower_price: lower_outer,
    };

    vec![upper, center, lower]
}

