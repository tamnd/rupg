%token ID NUM
%right '='
%left '+' '-'
%left '*'
%nonassoc '<'
%%
prog: /* empty */ | prog stmt ';' ;
stmt: ID { a(); } '=' expr { b(); } { c(); }
    | expr
    ;
expr: expr '+' expr | expr '-' expr | expr '*' expr | expr '<' expr
    | '-' expr %prec '*'
    | ID '=' expr
    | NUM
    | '(' expr ')'
    ;
