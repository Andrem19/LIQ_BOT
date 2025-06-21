pub fn simulate_short_pnl(
    notional_usd: f64,
    entry_price: f64,
    take_profit_pct: f64,
    stop_loss_pct: f64,
) -> (f64, f64) {
    // Объём шорта в базовом активе
    let size = notional_usd / entry_price;

    // Переводим проценты в реальные цены
    let tp_price = entry_price * (1.0 - take_profit_pct);
    let sl_price = entry_price * (1.0 + stop_loss_pct);

    // При тейк-профите: получили notional_usd, заплатили size * tp_price
    let usd_at_tp = notional_usd - size * tp_price;

    // При стоп-лоссе: получили notional_usd, заплатили size * sl_price
    let usd_at_sl = notional_usd - size * sl_price;

    (usd_at_tp, usd_at_sl)
}
pub fn simulate_short_power_pnl(
    notional_usd: f64,
    entry_price: f64,
    take_profit_pct: f64,
    stop_loss_pct: f64,
    power: f64,
) -> (f64, f64) {
    // 1. Рассчитываем "начальную" mark-цену контракта = entry_price^power
    let initial_mark = entry_price.powf(power);
    // 2. Объём контракта (в условных единицах Power Perp)
    let size = notional_usd / initial_mark;

    // 3. Цены базового актива для выхода по TP и SL
    let tp_index_price = entry_price * (1.0 - take_profit_pct);
    let sl_index_price = entry_price * (1.0 + stop_loss_pct);

    // 4. Переводим их в mark-цены Power Perp
    let tp_mark_price = tp_index_price.powf(power);
    let sl_mark_price = sl_index_price.powf(power);

    // 5. P&L для короткой позиции = коллатерал − стоимость закрытия позиции
    let pnl_tp = notional_usd - size * tp_mark_price;
    let pnl_sl = notional_usd - size * sl_mark_price;

    (pnl_tp, pnl_sl)
}
pub fn simulate_long_gamma_with_funding(
    margin_usd: f64,
    margin_per_mbtc: f64,
    entry_price: f64,
    down_pct: f64,
    up_pct: f64,
    daily_funding_per_mbtc: f64,
) -> (f64, f64, f64) {
    // 1) объём в mBTC-Gamma
    let size_mbtc = margin_usd / margin_per_mbtc;

    // 2) цены при отклонениях
    let price_down = entry_price * (1.0 - down_pct);
    let price_up   = entry_price * (1.0 + up_pct);

    // 3) абсолютные Δ
    let diff_down = entry_price - price_down;
    let diff_up   = price_up - entry_price;

    // 4) P&L: делим на 1 000 (mBTC = 0.001 BTC)
    let pnl_down = size_mbtc * diff_down.powi(2) / 1_000.0;
    let pnl_up   = size_mbtc * diff_up.powi(2)   / 1_000.0;

    // 5) funding
    let funding_day  = size_mbtc * daily_funding_per_mbtc;
    let funding_hour = funding_day / 24.0;

    (pnl_down, pnl_up, funding_hour)
}

pub fn simulate_put_pnl_qty(
    sol_amount: f64,      // сколько SOL вы хеджируете
    contract_price: f64,  // премия за 1 контракт (USD)
    spot_price: f64,      // текущая цена SOL (USD)
    lower_pct: f64,       // падение в долях (0.009 = 0.9%)
    upper_pct: f64,       // рост в долях
) -> (f64, f64) {
    // 1) число контрактов = sol_amount (1 контракт = 1 SOL)
    let contracts = sol_amount;

    // 2) цены при падении/росте
    let lower_price = spot_price * (1.0 - lower_pct);
    let upper_price = spot_price * (1.0 + upper_pct);

    // 3) strike = spot (ATM Put)
    let strike = spot_price;

    // 4) внутренняя стоимость
    let intrinsic_at_lower = (strike - lower_price).max(0.0);
    let intrinsic_at_upper = (strike - upper_price).max(0.0);

    // 5) PnL = (intrinsic - премия) * число контрактов
    let pnl_if_lower = contracts * (intrinsic_at_lower - contract_price);
    let pnl_if_upper = contracts * (intrinsic_at_upper - contract_price);

    (pnl_if_lower, pnl_if_upper)
}
#[derive(Debug, Clone)]
pub struct RangeAlloc {
    pub range_idx: usize,
    pub range_type: &'static str,
    pub usdc_amount: f64,
    pub sol_amount: f64,
    pub usdc_equivalent: f64,
    pub upper_price: f64,
    pub lower_price: f64,
}
#[derive(Debug, Clone, PartialEq)]
pub enum BoundType {
    Lower(usize),
    Base,
    Upper(usize),
}

#[derive(Debug, Clone)]
pub struct PriceBound {
    pub bound_type: BoundType,
    pub value: f64,
}

pub fn calc_bound_prices_struct(base_price: f64, pct_list: &[f64]) -> Vec<PriceBound> {
    assert!(
        pct_list.len() % 2 == 0,
        "pct_list.len() must be even: first n = ups, next n = downs"
    );
    let n = pct_list.len() / 2;

    // 1) Собираем нижние границы в порядке от ближней к дальней, затем развернём
    let mut lower = Vec::with_capacity(n);
    let mut price = base_price;
    for i in 0..n {
        let down_pct = pct_list[n + i];
        price *= 1.0 - down_pct;
        // numbered from 1..n
        lower.push(PriceBound {
            bound_type: BoundType::Lower(i + 1),
            value: price,
        });
    }
    lower.reverse(); // теперь Lower(n) ... Lower(1)

    // 2) Базовая цена
    let mut bounds = Vec::with_capacity(2 * n + 1);
    bounds.extend(lower);
    bounds.push(PriceBound {
        bound_type: BoundType::Base,
        value: base_price,
    });

    // 3) Собираем верхние границы в порядке от ближней к дальней
    let mut price = base_price;
    for i in 0..n {
        let up_pct = pct_list[i];
        price *= 1.0 + up_pct;
        bounds.push(PriceBound {
            bound_type: BoundType::Upper(i + 1),
            value: price,
        });
    }

    bounds
}

pub fn calc_range_allocation_struct(
    price: f64,
    bounds: &[PriceBound],
    weights: &[f64],
    total_usdc: f64,
) -> Vec<RangeAlloc> {
    // Отфильтруем точку Base — она не создаёт отдельного диапазона
    let mut eff: Vec<PriceBound> = bounds
        .iter()
        .filter(|b| b.bound_type != BoundType::Base)
        .cloned()
        .collect();
    // Убедимся, что границы отсортированы по возрастанию цены
    eff.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap());

    // Число диапазонов = число отфильтрованных границ - 1
    let m = eff.len().checked_sub(1)
        .expect("bounds.len() must be > 1 after filtering Base");
    assert!(
        weights.len() == m,
        "Ожидается {} весов (по одному на диапазон), получили {}",
        m, weights.len()
    );

    let mut allocs = Vec::with_capacity(m);
    for i in 0..m {
        let lower_price = eff[i].value;
        let upper_price = eff[i + 1].value;
        let weight_usdc = total_usdc * weights[i] / 100.0;

        // Выбираем, в какие токены вложить
        let (usdc_amount, sol_amount) = if price < lower_price {
            // все в SOL
            (0.0, weight_usdc / price)
        } else if price > upper_price {
            // все в USDC
            (weight_usdc, 0.0)
        } else {
            // внутри центрального диапазона — 50/50
            let half = weight_usdc / 2.0;
            (half, half / price)
        };

        let usdc_equivalent = usdc_amount + sol_amount * price;
        allocs.push(RangeAlloc {
            range_idx:      i,
            range_type:     "range",
            usdc_amount,
            sol_amount,
            usdc_equivalent,
            lower_price,
            upper_price,
        });
    }

    allocs
}



pub fn simulate_extremes(
    base_price: f64,
    pct_list: &[f64],
    weights: &[f64],
    total_usdc: f64,
) -> (f64, f64) {
    // 1. Вычисляем динамические границы
    let bounds = calc_bound_prices_struct(base_price, pct_list);
    // 2. Распределяем депозиты по диапазонам
    let allocs = calc_range_allocation_struct(base_price, &bounds, weights, total_usdc);

    // 3. Определяем крайние цены
    let lower_extreme = bounds.first().expect("bounds не может быть пустым").value;
    let upper_extreme = bounds.last().expect("bounds не может быть пустым").value;

    // 4. Подготавливаем данные по каждой позиции
    struct Deployment {
        lower:        f64,
        upper:        f64,
        L:            f64,
        leftover_sol: f64,
        leftover_usdc: f64,
    }
    let sqrt_base = base_price.sqrt();
    let deployments: Vec<Deployment> = allocs.into_iter().map(|alloc| {
        let p_low    = alloc.lower_price;
        let p_high   = alloc.upper_price;
        let sqrt_low = p_low.sqrt();
        let sqrt_high= p_high.sqrt();

        // 4.1. Считаем L и использованные токены
        let (L, used_sol, used_usdc) = if alloc.sol_amount > 0.0 && alloc.usdc_amount == 0.0 {
            // Только SOL
            let L = alloc.sol_amount * sqrt_low * sqrt_high / (sqrt_high - sqrt_low);
            (L, alloc.sol_amount, 0.0)
        } else if alloc.usdc_amount > 0.0 && alloc.sol_amount == 0.0 {
            // Только USDC
            let L = alloc.usdc_amount / (sqrt_high - sqrt_low);
            (L, 0.0, alloc.usdc_amount)
        } else {
            // Смешанная
            let L0 = alloc.sol_amount * (sqrt_base * sqrt_high) / (sqrt_high - sqrt_base);
            let L1 = alloc.usdc_amount / (sqrt_base - sqrt_low);
            let L  = L0.min(L1);
            let used_sol  = L * (sqrt_high - sqrt_base) / (sqrt_base * sqrt_high);
            let used_usdc = L * (sqrt_base - sqrt_low);
            (L, used_sol, used_usdc)
        };

        // 4.2. Остатки вне AMM
        let leftover_sol  = alloc.sol_amount  - used_sol;
        let leftover_usdc = alloc.usdc_amount - used_usdc;

        Deployment { lower: p_low, upper: p_high, L, leftover_sol, leftover_usdc }
    }).collect();

    // 5. Подсчёт P&L при заданной цене
    let calc_total = |price_end: f64| -> f64 {
        let sqrt_end = price_end.sqrt();
        deployments.iter().map(|d| {
            let sqrt_low  = d.lower.sqrt();
            let sqrt_high = d.upper.sqrt();

            // 5.1. P&L по ликвидности L
            let main = if price_end >= d.upper {
                // Всё в USDC
                d.L * (sqrt_high - sqrt_low)
            } else if price_end <= d.lower {
                // Всё в SOL, затем в USDC
                let a_sol = d.L * (sqrt_high - sqrt_low) / (sqrt_low * sqrt_high);
                a_sol * price_end
            } else {
                // Внутри диапазона
                let a_sol  = d.L * (sqrt_high - sqrt_end) / (sqrt_end * sqrt_high);
                let a_usdc = d.L * (sqrt_end  - sqrt_low);
                a_usdc + a_sol * price_end
            };

            // 5.2. Добавляем остатки
            main + d.leftover_usdc + d.leftover_sol * price_end
        }).sum()
    };

    let usdc_at_upper = calc_total(upper_extreme);
    let usdc_at_lower = calc_total(lower_extreme);
    (usdc_at_upper, usdc_at_lower)
}



fn enumerate_weights(
    idx: usize,
    current: &mut Vec<usize>,
    sum: usize,
    n: usize,
    min: usize,
    max: usize,
    callback: &mut dyn FnMut(&[usize]),
) {
    if idx + 1 == n {
        let w = 100 - sum;
        if w >= min && w <= max {
            current.push(w);
            callback(&current);
            current.pop();
        }
    } else {
        for w in min..=max {
            let rem = n - idx - 1;
            let min_rem = rem * min;
            let max_rem = rem * max;
            if sum + w + min_rem > 100 { break; }
            if sum + w + max_rem < 100 { continue; }
            current.push(w);
            enumerate_weights(idx + 1, current, sum + w, n, min, max, callback);
            current.pop();
        }
    }
}


fn main() {
    // =========================
    // Параметры симуляции
    // =========================
    let base_price: f64 = 150.0;
    let total_usdc:  f64 = 1000.0;

    // === Уровни границ (3 вверх, 3 вниз → 6 границ) ===
    let pct_list: Vec<f64> = vec![
        0.002, 0.014,  // три «вверх»
        0.005, 0.003,  // три «вниз»
    ];

    // 1) Считаем все границы
    let bounds = calc_bound_prices_struct(base_price, &pct_list);

    // 2) Фильтруем Base и считаем число диапазонов
    let eff_bounds: Vec<PriceBound> = bounds
        .iter()
        .filter(|b| b.bound_type != BoundType::Base)
        .cloned()
        .collect();
    let n_ranges = eff_bounds.len() - 1;
    println!("Detected {} ranges (excluding Base)", n_ranges);

    // === Одиночный тест с равными весами ===
    // let eq_weight = 100.0 / (n_ranges as f64);
    let weights_eq: Vec<f64> = vec![10.0,25.0,65.0];
    println!("\nSingle-test weights: {:?}", weights_eq);

    let allocs = calc_range_allocation_struct(base_price, &bounds, &weights_eq, total_usdc);
    println!("\nRange allocations (single test):");
    for a in &allocs {
        println!(
            "  [{}] {:.6} → {:.6} | usdc: {:10.6}, sol: {:10.6}, eq: {:10.6}",
            a.range_idx, a.lower_price, a.upper_price,
            a.usdc_amount, a.sol_amount, a.usdc_equivalent
        );
    }
    let (usdc_up_eq, usdc_down_eq) =
        simulate_extremes(base_price, &pct_list, &weights_eq, total_usdc);
    println!(
        "USDC @ upper extreme: {:.6}, @ lower extreme: {:.6}",
        usdc_up_eq, usdc_down_eq
    );
    // let (deriv_pnl_tp, deriv_pnl_sl, funding_per_hour) = simulate_long_gamma_with_funding(50.0, 72300.0, 105197.0, 0.02, 0.02, 8106.0);
    println!(
        "USDC delta: {:.6}",
        (usdc_up_eq-total_usdc)-(total_usdc-usdc_down_eq)
    );
    // let (deriv_pnl_tp, deriv_pnl_sl) = simulate_put_pnl_qty(2.0, 5.0, 150.0, 0.05, 0.05);
    

    // println!(
    //         "deriv_pnl: {:.6}-{:.6}",
    //         deriv_pnl_tp, deriv_pnl_sl
    //     );
    // =========================
    // Полный перебор весов
    // =========================
    println!("\n=== Поиск оптимальных весов ([10;40], сумма=100) ===");

    let min_w = 20;
    let max_w = 40;
    let mut best_score     = f64::NEG_INFINITY;
    let mut best_weights   = Vec::new();
    let mut best_usdc_up   = 0.0;
    let mut best_usdc_down = 0.0;

    // Буфер для одного кандидата
    let mut candidate = Vec::with_capacity(n_ranges);

    // Рекурсивно перебираем вектора длины n_ranges
    fn enumerate_weights(
        idx: usize,
        current: &mut Vec<usize>,
        sum: usize,
        n: usize,
        min: usize,
        max: usize,
        callback: &mut dyn FnMut(&[usize]),
    ) {
        if idx + 1 == n {
            let w = 100 - sum;
            if w >= min && w <= max {
                current.push(w);
                callback(&current);
                current.pop();
            }
        } else {
            for w in min..=max {
                let rem = n - idx - 1;
                let min_rem = rem * min;
                let max_rem = rem * max;
                if sum + w + min_rem > 100 { break; }
                if sum + w + max_rem < 100 { continue; }
                current.push(w);
                enumerate_weights(idx + 1, current, sum + w, n, min, max, callback);
                current.pop();
            }
        }
    }

    enumerate_weights(0, &mut candidate, 0, n_ranges, min_w, max_w, &mut |w_usize| {
        let weights_f64: Vec<f64> = w_usize.iter().map(|&x| x as f64).collect();
        let (usdc_up, usdc_down) =
            simulate_extremes(base_price, &pct_list, &weights_f64, total_usdc);
        let score = usdc_up + usdc_down;
        if score > best_score {
            best_score     = score;
            best_weights   = weights_f64.clone();
            best_usdc_up   = usdc_up;
            best_usdc_down = usdc_down;
        }
    });

    println!("\nBest weights: {:?}", best_weights);
    println!("  USDC @ upper extreme: {:.6}", best_usdc_up);
    println!("  USDC @ lower extreme: {:.6}", best_usdc_down);
    println!("  Sum (up+down):        {:.6}", (best_usdc_up-1000.0)-(1000.0-best_usdc_down));
}