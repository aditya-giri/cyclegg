use std::{collections::VecDeque};
use egg::{*};

#[path = "./ast.rs"] pub mod ast;
use ast::{*};

// We will use SymbolLang e-graphs with no analysis for now
pub type Eg = EGraph<SymbolLang, ()>;
pub type Rw = Rewrite<SymbolLang, ()>;

const WILDCARD: &str = "?x";

// How many times can a variable be case-split?
const MAX_SPLIT_DEPTH: usize = 2;

// Cost function to find a term that contains a given variable
struct HasVar(Symbol);
impl CostFunction<SymbolLang> for HasVar {
    type Cost = i32;
    fn cost<C>(&mut self, enode: &SymbolLang, mut costs: C) -> Self::Cost
    where
        C: FnMut(Id) -> Self::Cost
    {
        let op_cost = if enode.op == self.0 { -1 } else { 0 };
        enode.fold(op_cost, |m, id| std::cmp::min(m, costs(id)))
    }
}

// Condition that checks whether the substitution is into a smaller variable
struct SmallerVar(Symbol);
impl Condition<SymbolLang, ()> for SmallerVar {
  fn check(&self, egraph: &mut Eg, _eclass: Id, subst: &Subst) -> bool {
    let target_id = subst.get(WILDCARD.parse().unwrap()).unwrap().clone();
    let extractor = Extractor::new(egraph, AstSize);
    let (_, expr) = extractor.find_best(target_id); // TODO: this is incomplete, we actually need "are any of the expressions in this class smaller?"
    let res = is_descendant(&expr.to_string(), &self.0.to_string());
    println!("comparing {} < {}: {}", expr, self.0, res);
    res
  }
}

// Proof goal
pub struct Goal<'a> {
  pub name: String,
  // Equivalences we already proved
  pub egraph: Eg,
  // Rewrites that are valid for the current goal
  rewrites: Vec<Rw>,
  // Context
  ctx: Context,
  // Variables we haven't case-split on yet
  scrutinees: VecDeque<Symbol>,
  // Our goal is to prove lhs == rhs
  lhs: &'a Expr,
  rhs: &'a Expr,
  // Environment
  env: &'a Env,
}

// A proof state is a list of subgoals,
// all of which have to be discharged
pub type ProofState<'a> = Vec<Goal<'a>>;

impl<'a> Goal<'a> {
  pub fn top(      
    lhs: &'a Expr,
    rhs: &'a Expr,
    env: &'a Env,
    ctx: &Context,
    rewrites: &[Rw],    
    scrutinees: &[Symbol],
  ) -> Self {
    let mut egraph: Eg = Default::default();
    egraph.add_expr(&lhs);
    egraph.add_expr(&rhs);
    egraph.rebuild();
    Self {
      name: "top".to_string(),
      egraph,
      rewrites: rewrites.to_vec(),
      ctx: ctx.clone(),
      scrutinees: scrutinees.to_vec().into_iter().collect(),
      lhs,
      rhs,
      env,
    }}

  // Have we proven that lhs == rhs?
  pub fn done(&self) -> bool {
    !self.egraph.equivs(self.lhs, self.rhs).is_empty()
  }

  pub fn can_split(&self) -> bool {
    !self.scrutinees.is_empty()
  }

  // Saturate the goal by applying all available rewrites
  pub fn saturate(mut self) -> Self {
    let runner = Runner::default().with_egraph(self.egraph).run(self.rewrites.iter());
    self.egraph = runner.egraph;
    self
  }

  // Create a rewrite `lhs => rhs` which will serve as the lemma ("induction hypothesis") for a cycle in the proof;
  // here lhs and rhs are patterns, created by replacing the scrutinee var with a wildcard;
  // soundness requires that the pattern only applies to variables smaller than var.
  fn mk_lemma_rewrite(&self, var: Symbol) -> Option<Rw> {
    let extractor = Extractor::new(&self.egraph, HasVar(var));
    
    // Search for two expressions that are equivalent to the goals' lhs and rhs and that contain var
    let lhs_id = self.egraph.lookup_expr(self.lhs).unwrap();
    let (_, lhs_expr) = extractor.find_best(lhs_id);
    let rhs_id = self.egraph.lookup_expr(self.rhs).unwrap();
    let (_, rhs_expr) = extractor.find_best(rhs_id);
    
    let name = format!("lemma-{}", var);
    let lhs_pattern = lhs_expr.to_string().replace(var.as_str(), WILDCARD); // TODO: this is sketchy
    let rhs_pattern = rhs_expr.to_string().replace(var.as_str(), WILDCARD);
    // If neither pattern contains the wildcard, we cannot create the rewrite
    if !lhs_pattern.contains(WILDCARD) && !rhs_pattern.contains(WILDCARD) {
      return None;
    }

    let searcher: Pattern<SymbolLang> = lhs_pattern.parse().unwrap();
    let applier: Pattern<SymbolLang> = rhs_expr.to_string().replace(var.as_str(), WILDCARD).parse().unwrap();
    let condition = SmallerVar(var);

    println!("creating lemma: {} => {}", searcher, applier);
    let lemma = Rewrite::new(name, searcher, ConditionalApplier {condition: condition, applier: applier});
    
    lemma.ok()
  }

  // Consume this goal and add its case splits to the proof state
  pub fn case_split(mut self, state: &mut ProofState<'a>) {
    // Get the next variable to case-split on
    let var = self.scrutinees.pop_front().unwrap();
    println!("case-split on {}", var);
    let var_id = self.egraph.lookup(SymbolLang::leaf(var)).unwrap();

    let option_lemma = self.mk_lemma_rewrite(var);

    // Get the type of the variable
    let ty = self.ctx.get(&var).unwrap();
    // Convert to datatype name
    let dt = Symbol::from(ty.datatype().unwrap());
    // Get the constructors of the datatype
    let cons = self.env.get(&dt).unwrap();
    // For each constructor, create a new goal and push it onto the proof state
    // (we process constructors in reverse order so that base case end up at the top of the stack)
    for &con in cons.iter().rev() {
      let mut new_goal = Goal {
        name: if self.name == "top" { String::default() } else { format!("{}:", self.name) },
        egraph: self.egraph.clone(),
        rewrites: self.rewrites.clone(),
        ctx: self.ctx.clone(),
        scrutinees: self.scrutinees.clone(),
        lhs: self.lhs,
        rhs: self.rhs,
        env: self.env,
      };

      // Get the types of constructor arguments
      let con_args = self.ctx.get(&con).unwrap().args();
      // For each argument: create a fresh variable and add it to the context and to scrutinees
      let mut fresh_vars = vec![];
      for i in 0..con_args.len() {
        let fresh_var_name = format!("{}-{}{}", var, self.egraph.total_size(), i);        
        let depth = var_depth(&fresh_var_name[..]);
        let fresh_var = Symbol::from(fresh_var_name);        
        fresh_vars.push(fresh_var);
        // Add new variable to context
        new_goal.ctx.insert(fresh_var, con_args[i].clone());
        // Only add new variable to scrutinees if its depth doesn't exceed MAX_DEPTH        
        if depth < MAX_SPLIT_DEPTH {
          new_goal.scrutinees.push_back(fresh_var);
        }                
      }

      // Create an application of the constructor to the fresh vars
      let con_app_string = format!("({} {})", con, fresh_vars.iter().map(|x| x.to_string()).collect::<Vec<String>>().join(" "));
      let con_app: Expr = con_app_string.parse().unwrap();

      new_goal.name = format!("{}{}={}", new_goal.name, var, con_app);

      // Add con_app to the new goal's egraph and union it with var
      new_goal.egraph.add_expr(&con_app);
      let con_app_id = new_goal.egraph.lookup_expr(&con_app).unwrap();
      new_goal.egraph.union(var_id, con_app_id);
      new_goal.egraph.rebuild();

      // If we have a lemma, add it to the new goal's rewrites
      if let Some(lemma) = option_lemma.clone() {
        new_goal.rewrites.push(lemma);
      }      

      // Add the subgoal to the proof state
      state.push(new_goal);
    }

  }
}

pub fn pretty_state(state: &ProofState) -> String {
  format!("[{}]", state.iter().map(|g| g.name.clone()).collect::<Vec<String>>().join(", "))
}
